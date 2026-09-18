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
static ABSOLUTE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z][a-zA-Z0-9+.-]*:").unwrap());

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

/// The file a `src`/`href` names, when it is one we could stamp: same-origin, and inside `root`.
fn resolve_local_script(root: &Path, html_dir: &Path, url: &str) -> Option<PathBuf> {
    // A URL with a scheme, or protocol-relative, is somebody else's file (a CDN): its bytes are not
    // ours to change, so stamping cannot invalidate its hash.
    if url.starts_with("//") || ABSOLUTE_URL.is_match(url) {
        return None;
    }
    // Browsers strip surrounding whitespace from a URL attribute, so `src=" main.js "` loads it.
    let path_part = url.split(['?', '#']).next().unwrap_or("").trim();
    if path_part.is_empty() {
        return None;
    }
    let candidate = if let Some(rooted) = path_part.strip_prefix('/') {
        // A root-relative `/assets/app.js` is served from the output root.
        root.join(rooted)
    } else {
        html_dir.join(path_part)
    };
    let full = normalize(&candidate);
    let base = normalize(root);
    // `starts_with` on COMPONENTS, so `dist-2` is not read as inside `dist`.
    full.starts_with(&base).then_some(full)
}

/// Lexical `..`/`.` resolution — the file may not exist yet, so `canonicalize` is not an option.
fn normalize(path: &Path) -> PathBuf {
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

fn is_js(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("js") | Some("cjs") | Some("mjs")
    )
}

/// Every script under `roots` whose hash an HTML page there pins.
///
/// Empty means stamping is safe as far as SRI is concerned. Never fails on a path problem: that is
/// the caller's to report.
pub fn find_pinned_scripts(roots: &[PathBuf]) -> Vec<PinnedScript> {
    let mut found = BTreeSet::new();
    for root in roots {
        // A file argument (`inject dist/app.js`) has no HTML of its own; scan its directory.
        let base = if root.is_file() {
            root.parent().unwrap_or(Path::new(".")).to_path_buf()
        } else {
            root.clone()
        };
        for entry in walkdir::WalkDir::new(&base)
            .max_depth(8)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| {
                // `e.depth() > 0`: the filter must not judge the ROOT the caller named. A build
                // output legitimately lives in a dot-directory (`.next`, `.nuxt`, `.output`, and a
                // `tempfile` fixture), and excluding it would silently scan nothing at all.
                let name = e.file_name().to_string_lossy();
                !(e.depth() > 0
                    && e.file_type().is_dir()
                    && (name == "node_modules" || name.starts_with('.')))
            })
            .filter_map(Result::ok)
        {
            let page = entry.path();
            if !entry.file_type().is_file() {
                continue;
            }
            let is_html = matches!(
                page.extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("html") | Some("htm") | Some("xhtml")
            );
            if !is_html {
                continue;
            }
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
                let Some(script) = resolve_local_script(&base, html_dir, unquote(url.as_str()))
                else {
                    continue;
                };
                if is_js(&script) {
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

    fn pinned(dir: &Path) -> Vec<PathBuf> {
        find_pinned_scripts(&[dir.to_path_buf()])
            .into_iter()
            .map(|p| p.script)
            .collect()
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

        let found = find_pinned_scripts(&[dir.path().to_path_buf()]);
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

        assert_eq!(find_pinned_scripts(&[out]), Vec::new());
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

        let found = find_pinned_scripts(&[js]);
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
            find_pinned_scripts(&[dir.path().join("does-not-exist")]),
            Vec::new()
        );
    }

    /// `node_modules` and dot-directories inside a build output are not the app's pages.
    #[test]
    fn it_skips_node_modules_and_dot_directories() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "main.js", "1");
        write(
            dir.path(),
            "node_modules/pkg/demo.html",
            r#"<script src="/main.js" integrity="sha384-M"></script>"#,
        );
        write(
            dir.path(),
            ".cache/page.html",
            r#"<script src="/main.js" integrity="sha384-C"></script>"#,
        );

        assert_eq!(pinned(dir.path()), Vec::<PathBuf>::new());
    }
}
