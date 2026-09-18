//! Refuse to stamp a build that pins its own script hashes (Subresource Integrity).
//!
//! `inject` appends the debug-id comment and the runtime registration to every `.js` it finds. A
//! build that computed SRI hashes during emit — `webpack-subresource-integrity`, Angular's
//! `subresourceIntegrity: true` — has already written a hash of the PRE-stamp bytes into its HTML,
//! so the browser refuses the script and the page runs NOTHING.
//!
//! Measured (bugsee-javascript, 2026-09-18) on a real webpack 5.111 build served over HTTP and
//! loaded in Chromium 151: before inject the app ran clean; after it, `window.__ran` was false and
//! the console carried "Failed to find a valid digest in the 'integrity' attribute for resource
//! '…/main.<hash>.js' … The resource has been blocked." `index.html` was byte-identical; only the
//! JS grew, 114 → 472 bytes.
//!
//! The JS bundler plugins carry the same guard, but they only cover vite and rollup: Angular 17+,
//! esbuild and Deno users drive this binary directly, and Angular is exactly the config this
//! protects against.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use std::sync::LazyLock;

use regex::Regex;

/// One `<script>`/`<link>` whose `integrity` pins a file we would stamp.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PinnedScript {
    /// The HTML page carrying the attribute.
    pub html: PathBuf,
    /// The script it pins.
    pub script: PathBuf,
}

static SCRIPT_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<script\b[^>]*>").unwrap());
static LINK_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<link\b[^>]*>").unwrap());
static HTML_COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").unwrap());
// A failed `modulepreload` poisons the module map, so the later `import()` of that chunk fails with
// it. `prefetch` is deliberately absent: a failed prefetch is discarded, not fatal.
static PRELOADS_SCRIPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\brel\s*=\s*["']?(modulepreload|preload)"#).unwrap());
// `[\s"'<]` rather than `\b`, so `data-integrity` / `data-src` (a framework's own bookkeeping,
// pinning nothing) do not count.
static INTEGRITY_ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)[\s"'<]integrity\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)"#).unwrap()
});
static SRC_ATTR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)[\s"'<]src\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)"#).unwrap());
static HREF_ATTR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)[\s"'<]href\s*=\s*("[^"]*"|'[^']*'|[^\s>]+)"#).unwrap());
// `https:`, `//cdn…` — the origin half of a URL, which has no counterpart on disk. What follows it
// still can: `publicPath` pointing at a CDN is the canonical SRI deployment.
static ABSOLUTE_URL_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-zA-Z][a-zA-Z0-9+.-]*:)?//").unwrap());

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// The file a `src`/`href` names, resolved against the files this run would actually stamp.
///
/// `targets` is the whole point: the only hash we can invalidate is one belonging to a file we are
/// going to rewrite. Matching against the file system instead produced both kinds of error — a stale
/// page pinning a deleted bundle refused the run, while a page whose URL did not resolve literally
/// (a CDN `publicPath`) sailed through and the build shipped blank.
fn resolve_pinned_target(
    roots: &[PathBuf],
    targets: &BTreeSet<PathBuf>,
    html_dir: &Path,
    url: &str,
) -> Option<PathBuf> {
    // Browsers strip surrounding whitespace from a URL attribute, so `src=" main.js "` loads it.
    let trimmed = url.trim();
    let path_part = if let Some(rest) = ABSOLUTE_URL_PREFIX.find(trimmed) {
        // `https://cdn.example.com/assets/main.abc.js` — webpack's `output.publicPath` pointing at a
        // CDN is the CANONICAL SRI deployment (hash the local bytes, serve them from the CDN), and
        // those bytes are the ones sitting in the output directory. Keep the path, drop the origin.
        let after_scheme = &trimmed[rest.end()..];
        after_scheme.split_once('/').map(|(_, p)| p).unwrap_or("")
    } else {
        trimmed
    };
    let path_part = path_part.split(['?', '#']).next().unwrap_or("").trim();
    if path_part.is_empty() {
        return None;
    }

    // 1. Where the URL literally points, relative to the page (or to the root when root-relative).
    let literal = if let Some(rooted) = path_part.strip_prefix('/') {
        roots
            .iter()
            .map(|r| lexical_normalize(&r.join(rooted)))
            .collect::<Vec<_>>()
    } else {
        vec![lexical_normalize(&html_dir.join(path_part))]
    };
    if let Some(hit) = literal.into_iter().find(|p| targets.contains(p)) {
        return Some(hit);
    }

    // 2. Otherwise by file name. A `publicPath` — `/static/`, `/_next/`, a CDN origin — puts a
    //    prefix in the URL that has no counterpart on disk, so the literal path resolves to nothing
    //    while the pinned bytes are very much in the output. Bundler file names carry a content
    //    hash, so a collision is unlikely; and refusing a build we would not have broken costs
    //    symbolication, while missing one ships a page that loads nothing.
    let name = Path::new(path_part).file_name()?;
    targets
        .iter()
        .find(|t| t.file_name() == Some(name))
        .cloned()
}

/// Lexical `..`/`.` resolution — the path may not exist, so `canonicalize` is not an option.
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Pages sitting DIRECTLY in `dir` (not recursive, and bounded).
///
/// Only consulted when the path we were given holds no pages of its own: the standard Vite/webpack
/// layout is `dist/index.html` + `dist/assets/*.js`, so `inject dist/assets` (or one bundle by path)
/// would otherwise stamp a file whose hash the page one level up pins. The bound matters because
/// that parent can be a directory nobody meant us to read — `/tmp` with a hundred thousand entries.
fn pages_directly_in(dir: Option<&Path>) -> Vec<PathBuf> {
    const MAX_ENTRIES: usize = 2_000;
    let Some(dir) = dir else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut pages: Vec<PathBuf> = entries
        .take(MAX_ENTRIES)
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()) && is_html(&e.path()))
        .map(|e| e.path())
        .collect();
    pages.sort();
    pages
}

/// A file name a browser would load as a page.
fn is_html(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("html") | Some("htm") | Some("xhtml")
    )
}

/// Every file this run would stamp whose hash an HTML page under `roots` pins.
///
/// Empty means stamping is safe as far as SRI is concerned. Never fails on a path problem: that is
/// the caller's to report.
pub fn find_pinned_scripts(roots: &[PathBuf], targets: &BTreeSet<PathBuf>) -> Vec<PinnedScript> {
    if targets.is_empty() {
        return Vec::new();
    }
    let mut found = BTreeSet::new();
    for root in roots {
        // A file argument (`inject dist/app.js`) has no HTML of its own; scan its directory.
        let base = if root.is_file() {
            root.parent().unwrap_or(Path::new(".")).to_path_buf()
        } else {
            root.clone()
        };
        // Everything under the root, plus the pages sitting DIRECTLY in its parent. The standard
        // Vite/webpack layout is `dist/index.html` + `dist/assets/*.js`, so `inject dist/assets`
        // (or a single bundle by path) would otherwise stamp a file whose hash the page one level
        // up pins — a build we break while reporting success. One level, not recursive: enough for
        // that layout without wandering off into a parent tree we were not pointed at.
        //
        // No directory filter and no depth cap below the root: the WALK has none either, so a page
        // under `.vitepress/dist` or `node_modules` pins a file we really are going to stamp.
        // (Skipping those was a false negative the first draft shipped.)
        let mut pages: Vec<PathBuf> = walkdir::WalkDir::new(&base)
            .sort_by_file_name()
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file() && is_html(e.path()))
            .map(|e| e.path().to_path_buf())
            .collect();
        if pages.is_empty() {
            pages = pages_directly_in(base.parent());
        }
        for page in pages {
            let page = page.as_path();
            let Ok(source) = std::fs::read_to_string(page) else {
                continue; // a page we cannot read is not ours to fail the run over
            };
            let markup = HTML_COMMENT.replace_all(&source, "");
            let html_dir = page.parent().unwrap_or(Path::new("."));
            let tags = SCRIPT_TAG
                .find_iter(&markup)
                .map(|m| (m.as_str(), &*SRC_ATTR))
                .chain(
                    LINK_TAG
                        .find_iter(&markup)
                        .filter(|m| PRELOADS_SCRIPT.is_match(m.as_str()))
                        .map(|m| (m.as_str(), &*HREF_ATTR)),
                );
            for (tag, url_attr) in tags {
                let integrity = INTEGRITY_ATTR
                    .captures(tag)
                    .and_then(|c| c.get(1))
                    .map(|m| unquote(m.as_str()).trim().to_string());
                // An empty `integrity` pins nothing — browsers treat it as no check at all.
                if integrity.as_deref().unwrap_or("").is_empty() {
                    continue;
                }
                let Some(url) = url_attr.captures(tag).and_then(|c| c.get(1)) else {
                    continue;
                };
                if let Some(script) =
                    resolve_pinned_target(roots, targets, html_dir, unquote(url.as_str()))
                {
                    found.insert(PinnedScript {
                        html: page.to_path_buf(),
                        script,
                    });
                }
            }
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let full = dir.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, body).unwrap();
        full
    }

    /// The files an `inject` over `dir` would stamp — every `.js`/`.cjs`/`.mjs` under it. In
    /// production this list comes from the same walk that does the stamping, which is the point:
    /// the guard can only refuse over a file that is really going to be rewritten.
    fn targets_of(dirs: &[&Path]) -> BTreeSet<PathBuf> {
        dirs.iter()
            .flat_map(|dir| {
                walkdir::WalkDir::new(dir)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_file())
                    .map(|e| e.path().to_path_buf())
            })
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("js") | Some("cjs") | Some("mjs")
                )
            })
            .collect()
    }

    fn pinned_in(dir: &Path, targets: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
        find_pinned_scripts(&[dir.to_path_buf()], targets)
            .into_iter()
            .map(|p| p.script)
            .collect()
    }

    fn pinned(dir: &Path) -> Vec<PathBuf> {
        pinned_in(dir, &targets_of(&[dir]))
    }

    /// The shape that breaks: webpack-subresource-integrity + html-webpack-plugin.
    #[test]
    fn a_pinned_entry_script_is_found() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "main.abc123.js", "console.log(1)");
        write(
            dir.path(),
            "index.html",
            "<!doctype html><script defer src=main.abc123.js integrity=sha384-KUBb crossorigin=anonymous></script>",
        );

        let found = find_pinned_scripts(&[dir.path().to_path_buf()], &targets_of(&[dir.path()]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].script, dir.path().join("main.abc123.js"));
        assert_eq!(found[0].html, dir.path().join("index.html"));
    }

    /// A failed `modulepreload` poisons the module map, so the later `import()` fails with it.
    /// Angular's builder and the Vite SRI plugins emit these.
    #[test]
    fn a_pinned_modulepreload_counts_but_a_prefetch_does_not() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "chunk.js", "1");
        write(dir.path(), "later.js", "2");
        write(
            dir.path(),
            "index.html",
            r#"<link rel="modulepreload" href="chunk.js" integrity="sha384-P">
               <link rel="prefetch" href="later.js" integrity="sha384-Q">"#,
        );

        assert_eq!(pinned(dir.path()), vec![dir.path().join("chunk.js")]);
    }

    /// Quoting, attribute order, root-relative paths, `.mjs`/`.cjs`, nested pages — one pass over
    /// the shapes bundlers actually emit.
    #[test]
    fn it_reads_the_shapes_bundlers_emit() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.mjs", "1");
        write(dir.path(), "assets/b.cjs", "2");
        write(dir.path(), "assets/c.js", "3");
        write(
            dir.path(),
            "index.html",
            r#"<script integrity="sha512-A" type="module" src='a.mjs'></script>
               <script src="/assets/b.cjs" integrity=sha256-B></script>"#,
        );
        write(
            dir.path(),
            "nested/page.html",
            r#"<script src="  ../assets/c.js  " INTEGRITY = "sha384-C"></script>"#,
        );

        let mut found = pinned(dir.path());
        found.sort();
        let mut want = vec![
            dir.path().join("a.mjs"),
            dir.path().join("assets/b.cjs"),
            dir.path().join("assets/c.js"),
        ];
        want.sort();
        assert_eq!(found, want);
    }

    /// Everything that must NOT stop a build. Each one would be a silent loss of symbolication for
    /// a user we were never going to break.
    #[test]
    fn it_does_not_fire_on_anything_we_would_not_break() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "main.js", "1");
        write(dir.path(), "app.css", "body{}");
        write(dir.path(), "app.wasm", "\0asm");
        write(dir.path(), "../sibling.js", "1");
        write(
            dir.path(),
            "index.html",
            r#"<script src="main.js"></script>
               <script src="https://cdn.example/lib.js" integrity="sha384-X"></script>
               <script src="//cdn.example/lib2.js" integrity="sha384-Y"></script>
               <script src="main.js" integrity=""></script>
               <script data-integrity="sha384-Z" data-src="main.js"></script>
               <script integrity="sha384-I">console.log('inline')</script>
               <script src="app.wasm" integrity="sha384-W"></script>
               <link rel="stylesheet" href="app.css" integrity="sha384-S">
               <!-- <script src="main.js" integrity="sha384-C"></script> -->"#,
        );

        assert_eq!(pinned(dir.path()), Vec::<PathBuf>::new());
    }

    /// A sibling directory sharing a prefix is NOT inside the output, and neither is a parent.
    #[test]
    fn containment_is_by_path_component_not_by_prefix() {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("dist");
        write(root.path(), "dist-2/main.js", "1");
        write(root.path(), "vendor.js", "2");
        write(
            &out,
            "index.html",
            r#"<script src="../dist-2/main.js" integrity="sha384-N"></script>
               <script src="../vendor.js" integrity="sha384-V"></script>"#,
        );

        assert_eq!(
            find_pinned_scripts(std::slice::from_ref(&out), &targets_of(&[&out])),
            Vec::new()
        );
    }

    /// Pointed at a FILE (`inject dist/app.js`), the scan still sees the page beside it — that is
    /// the invocation a hand-written build script uses.
    #[test]
    fn a_file_argument_still_scans_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let js = write(dir.path(), "app.js", "1");
        write(
            dir.path(),
            "index.html",
            r#"<script src="app.js" integrity="sha384-F"></script>"#,
        );

        let found = find_pinned_scripts(std::slice::from_ref(&js), &targets_of(&[dir.path()]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].script, dir.path().join("app.js"));
    }

    /// No HTML, an unreadable page, and a missing directory are all "nothing to report" — this
    /// guard must never be the thing that fails a run.
    #[test]
    fn it_is_silent_when_there_is_nothing_to_find() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "index.js", "1");
        assert_eq!(pinned(dir.path()), Vec::<PathBuf>::new());
        assert_eq!(
            find_pinned_scripts(
                &[dir.path().join("does-not-exist")],
                &targets_of(&[dir.path()])
            ),
            Vec::new()
        );
    }

    /// A page under a dot-directory or `node_modules` still pins a file the walk WILL stamp — and
    /// the walk descends both. The first draft skipped them here and shipped that false negative:
    /// a VitePress build (`docs/.vitepress/dist/index.html`) had its entry stamped and went blank.
    #[test]
    fn a_page_under_a_dot_directory_or_node_modules_still_counts() {
        for nested in [".vitepress/dist/index.html", "node_modules/pkg/index.html"] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), "main.js", "1");
            write(
                dir.path(),
                nested,
                r#"<script src="/main.js" integrity="sha384-M"></script>"#,
            );

            assert_eq!(
                pinned(dir.path()),
                vec![dir.path().join("main.js")],
                "page at {nested} was not seen"
            );
        }
    }

    /// The standard Vite/webpack layout keeps the page one level ABOVE the bundles
    /// (`dist/index.html` + `dist/assets/*.js`), so pointing `inject` at the assets directory — or
    /// at one bundle by path — used to miss the pin entirely and stamp the file anyway.
    #[test]
    fn a_page_one_level_above_the_given_path_still_counts() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "assets/main.abc.js", "1");
        write(
            dir.path(),
            "index.html",
            r#"<script src="/assets/main.abc.js" integrity="sha384-V"></script>"#,
        );

        let assets = dir.path().join("assets");
        let targets = targets_of(&[&assets]);
        assert_eq!(
            find_pinned_scripts(std::slice::from_ref(&assets), &targets)
                .into_iter()
                .map(|p| p.script)
                .collect::<Vec<_>>(),
            vec![dir.path().join("assets/main.abc.js")],
            "a page in the parent directory pins a file we would stamp"
        );

        // …and the same when a single bundle is named by path.
        let one = dir.path().join("assets/main.abc.js");
        assert_eq!(
            find_pinned_scripts(std::slice::from_ref(&one), &targets).len(),
            1
        );
    }

    /// `output.publicPath` pointing at a CDN is the CANONICAL SRI deployment — hash the local bytes,
    /// serve them from the CDN — so the URL carries an origin that exists nowhere on disk while the
    /// pinned bytes are the ones in the output directory. Dropping every absolute URL as "somebody
    /// else's file" shipped a blank page for exactly the setup SRI exists for.
    #[test]
    fn a_cdn_public_path_still_resolves_to_the_local_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "main.abc123.js", "1");
        write(
            dir.path(),
            "index.html",
            r#"<script src="https://cdn.example.com/assets/main.abc123.js" integrity="sha384-C"></script>"#,
        );

        assert_eq!(pinned(dir.path()), vec![dir.path().join("main.abc123.js")]);
    }

    /// The same shape one level down: a root-relative `publicPath` (`/static/`, `/_next/`) prefixes
    /// the URL with a directory that does not exist under the output root.
    #[test]
    fn a_root_relative_public_path_resolves_by_file_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "main.js", "1");
        write(
            dir.path(),
            "index.html",
            r#"<script src="/static/main.js" integrity="sha384-S"></script>"#,
        );

        assert_eq!(pinned(dir.path()), vec![dir.path().join("main.js")]);
    }

    /// The literal path wins over the file-name fallback: a build with `app.js` in two directories
    /// must resolve to the one the page actually points at, or the refusal names the wrong file and
    /// `--exclude`ing that file would not lift it.
    #[test]
    fn the_literal_path_decides_when_two_bundles_share_a_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "en/app.js", "1");
        write(dir.path(), "fr/app.js", "2");
        write(
            dir.path(),
            "fr/index.html",
            r#"<script src="app.js" integrity="sha384-F"></script>"#,
        );

        assert_eq!(pinned(dir.path()), vec![dir.path().join("fr/app.js")]);
    }

    /// …but a CDN script that is NOT part of this build stays ignored: nothing we stamp bears that
    /// name, so nothing we do can invalidate its hash.
    #[test]
    fn a_third_party_cdn_script_is_still_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "main.js", "1");
        write(
            dir.path(),
            "index.html",
            r#"<script src="https://cdn.example.com/jquery-3.7.1.min.js" integrity="sha384-J"></script>"#,
        );

        assert_eq!(pinned(dir.path()), Vec::<PathBuf>::new());
    }

    /// Only a file this run would STAMP can have its hash invalidated. A page pinning a bundle that
    /// no longer exists (a stale `index.html` from an earlier build), or one the caller excluded, or
    /// one outside the output entirely, must not stop the run.
    #[test]
    fn a_pin_on_something_we_will_not_stamp_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "main.js", "1");
        write(dir.path(), "vendor/pinned.js", "2");
        write(
            dir.path(),
            "index.html",
            r#"<script src="gone.OLD.js" integrity="sha384-G"></script>
               <script src="vendor/pinned.js" integrity="sha384-V"></script>"#,
        );

        // `vendor/pinned.js` is real but NOT in the target list — the caller excluded it.
        let targets: BTreeSet<PathBuf> = [dir.path().join("main.js")].into_iter().collect();
        assert_eq!(pinned_in(dir.path(), &targets), Vec::<PathBuf>::new());
    }

    /// A file argument stamps ONE file, so only a pin on that file can matter. Passing the unpinned
    /// bundle explicitly is the most direct way to follow the error's own advice.
    #[test]
    fn a_file_argument_only_counts_pins_on_that_file() {
        let dir = tempfile::tempdir().unwrap();
        let app = write(dir.path(), "app.js", "1");
        write(dir.path(), "main.js", "2");
        write(
            dir.path(),
            "index.html",
            r#"<script src="main.js" integrity="sha384-M"></script>"#,
        );

        let targets: BTreeSet<PathBuf> = [app.clone()].into_iter().collect();
        assert_eq!(
            find_pinned_scripts(std::slice::from_ref(&app), &targets),
            Vec::new(),
            "only main.js is pinned, and only app.js would be stamped"
        );

        // …and a pin on the file we ARE stamping still counts.
        write(
            dir.path(),
            "index.html",
            r#"<script src="app.js" integrity="sha384-A"></script>"#,
        );
        assert_eq!(
            find_pinned_scripts(std::slice::from_ref(&app), &targets).len(),
            1
        );
    }

    /// `.htm` and `.xhtml` are pages too, and a deep page is still a page: the walk that stamps has
    /// no depth limit, so neither can this.
    #[test]
    fn it_reads_htm_and_xhtml_and_deep_pages() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.js", "1");
        write(dir.path(), "b.js", "2");
        write(
            dir.path(),
            "legacy.htm",
            r#"<script src="/a.js" integrity="sha384-A"></script>"#,
        );
        write(
            dir.path(),
            "a/b/c/d/e/f/g/h/i/deep.xhtml",
            r#"<script src="/b.js" integrity="sha384-B"/>"#,
        );

        let mut found = pinned(dir.path());
        found.sort();
        let mut want = vec![dir.path().join("a.js"), dir.path().join("b.js")];
        want.sort();
        assert_eq!(found, want);
    }

    /// A comment spanning lines is still a comment (the `(?s)` flag), and `preload` counts as well
    /// as `modulepreload`.
    #[test]
    fn multiline_comments_and_both_preload_rels() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "commented.js", "1");
        write(dir.path(), "preloaded.js", "2");
        write(
            dir.path(),
            "index.html",
            "<!--\n<script src=\"commented.js\" integrity=\"sha384-C\"></script>\n-->\n             <link rel=\"preload\" as=\"script\" href=\"preloaded.js\" integrity=\"sha384-P\">",
        );

        assert_eq!(pinned(dir.path()), vec![dir.path().join("preloaded.js")]);
    }
}
