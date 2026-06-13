//! VCS metadata resolver.
//!
//! Detects the CI provider via environment variables (GitHub Actions /
//! GitLab CI / Bitbucket Pipelines) and resolves provider-aware fields:
//! `commit_sha`, `branch`, `base_branch`, `pr_number`, `provider`,
//! `repo`. Falls back to shelling out to `git` in the supplied working
//! directory when no CI provider matches — covers local archives and
//! any CI we don't specifically recognize.
//!
//! ## Cross-repo origin
//!
//! Ported from the Bugsee iOS SDK's `tools.bundle/BugseeAgent` (Python)
//! and the fastlane plugin's `BugseeAgent` (also Python) which had
//! near-identical implementations. Consolidating into bugsee-cli
//! eliminates the cross-language divergence cost — both Python sides
//! now shell to `bugsee-cli vcs-metadata` and consume the JSON output.
//!
//! The output wire shape MUST stay byte-compatible with what the
//! Bugsee Android Gradle plugin's `VcsMetadataResolver` emits and what
//! the appserver's `sanitizeVcs` accepts. Drift would silently break
//! the dashboard's branch / PR / commit attribution for builds dispatched
//! through this CLI.
//!
//! ## Resolution order (first provider with a positive signal wins)
//!
//! 1. **GitHub Actions** (env `GITHUB_ACTIONS=true|1|yes|on`)
//! 2. **GitLab CI** (env `GITLAB_CI=true|...`)
//! 3. **Bitbucket Pipelines** (env `BITBUCKET_BUILD_NUMBER` non-empty)
//! 4. **Git fallback** — shell `git rev-parse` in `working_dir`. Returns
//!    `{commit_sha, branch}` only; no `provider` / `repo` fields.
//!
//! No provider, no working_dir match, or `git` not on PATH → returns
//! an empty object (which the JSON reader treats as "no VCS metadata
//! available", same as fields all absent).

use clap::Args;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `bugsee-cli vcs-metadata` argument shape.
#[derive(Args, Debug)]
pub struct VcsMetadataArgs {
    /// Working directory the git fallback runs in. When no CI provider
    /// is detected, the resolver shells `git rev-parse HEAD` and
    /// `git rev-parse --abbrev-ref HEAD` in this directory. Defaults
    /// to the process CWD when omitted.
    #[arg(long)]
    pub working_dir: Option<PathBuf>,
}

/// VCS metadata wire shape. All fields are optional; absence in the
/// emitted JSON is signalled by `Option::None` paired with
/// `serde(skip_serializing_if = "Option::is_none")` so the back-end
/// distinguishes "unknown" from "known empty" cleanly.
///
/// Field names MUST match the Bugsee Android Gradle plugin's
/// `VcsMetadataResolver` output AND the appserver's `sanitizeVcs`
/// accept-list. A rename here is a cross-repo coordination event.
#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct VcsMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// CLI dispatch — resolve from the current process env + supplied
/// working dir, print the JSON to stdout, exit 0.
pub fn dispatch(args: VcsMetadataArgs) -> anyhow::Result<()> {
    let working_dir = args
        .working_dir
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let env: HashMap<String, String> = std::env::vars().collect();
    let metadata = resolve(&env, &working_dir);
    let json = serde_json::to_string(&metadata)?;
    println!("{}", json);
    Ok(())
}

/// Core resolver — visible for testing. `env` is the CI provider env
/// var map; `working_dir` is the git-fallback directory.
pub fn resolve(env: &HashMap<String, String>, working_dir: &Path) -> VcsMetadata {
    // GitHub Actions ──────────────────────────────────────────────
    if env_truthy(env.get("GITHUB_ACTIONS")) {
        let mut out = VcsMetadata {
            provider: Some("github"),
            ..Default::default()
        };
        set_if_present(&mut out.commit_sha, env.get("GITHUB_SHA"));
        set_if_present(&mut out.repo, env.get("GITHUB_REPOSITORY"));
        let event_name = env.get("GITHUB_EVENT_NAME").map(|s| s.as_str()).unwrap_or("");
        if event_name == "pull_request" {
            set_if_present(&mut out.branch, env.get("GITHUB_HEAD_REF"));
            set_if_present(&mut out.base_branch, env.get("GITHUB_BASE_REF"));
            // `refs/pull/<n>/merge` → 42
            if let Some(refs) = env.get("GITHUB_REF") {
                if let Some(n) = parse_github_pull_ref(refs) {
                    out.pr_number = Some(n);
                }
            }
        } else {
            // Push event: GITHUB_REF carries `refs/heads/<branch>` for
            // a branch push, `refs/tags/<tag>` for a tag push. Only
            // emit `branch` when the ref is a head ref. Tag-triggered
            // builds previously stored the literal `refs/tags/v1.0.0`
            // in the branch column — the Android Gradle plugin's
            // canonical Kotlin VcsMetadataResolver does NOT do that
            // (`removePrefix("refs/heads/").takeIf { it != ref }`).
            if let Some(refs) = env.get("GITHUB_REF") {
                if let Some(branch) = refs.strip_prefix("refs/heads/") {
                    set_if_present_raw(&mut out.branch, Some(branch));
                }
            }
        }
        return out;
    }

    // GitLab CI ──────────────────────────────────────────────────
    if env_truthy(env.get("GITLAB_CI")) {
        let mut out = VcsMetadata {
            provider: Some("gitlab"),
            ..Default::default()
        };
        set_if_present(&mut out.commit_sha, env.get("CI_COMMIT_SHA"));
        set_if_present(&mut out.repo, env.get("CI_PROJECT_PATH"));
        // Merge-request pipelines expose different branch variables
        // than push pipelines. `CI_MERGE_REQUEST_IID` is the MR number
        // (the `_ID` variant is a global DB id and unusable as a PR
        // reference).
        let mr_iid = env
            .get("CI_MERGE_REQUEST_IID")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        if let Some(iid) = mr_iid {
            set_if_present(&mut out.branch, env.get("CI_MERGE_REQUEST_SOURCE_BRANCH_NAME"));
            set_if_present(
                &mut out.base_branch,
                env.get("CI_MERGE_REQUEST_TARGET_BRANCH_NAME"),
            );
            if let Ok(n) = iid.parse::<u64>() {
                out.pr_number = Some(n);
            }
        } else {
            // Branch vs tag pipeline distinction. GitLab CI sets:
            //   - CI_COMMIT_BRANCH on branch pipelines (NOT tag).
            //   - CI_COMMIT_TAG on tag pipelines (NOT branch).
            //   - CI_COMMIT_REF_NAME is ALWAYS set; on a tag pipeline
            //     it equals the tag name, which used to land in the
            //     `branch` field verbatim — same bug class as the
            //     GitHub `refs/tags/<tag>` leak fixed in cf1325f.
            // Prefer CI_COMMIT_BRANCH when present; otherwise leave
            // `branch` absent so tag-triggered pipelines don't
            // mis-render the tag in the branch column.
            if env
                .get("CI_COMMIT_BRANCH")
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            {
                set_if_present(&mut out.branch, env.get("CI_COMMIT_BRANCH"));
            } else if env
                .get("CI_COMMIT_TAG")
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            {
                // Tag pipeline — leave branch absent.
            } else {
                // No specific branch/tag marker — fall back to
                // CI_COMMIT_REF_NAME for legacy GitLab versions
                // (pre-12.6) that didn't emit CI_COMMIT_BRANCH.
                set_if_present(&mut out.branch, env.get("CI_COMMIT_REF_NAME"));
            }
        }
        return out;
    }

    // Bitbucket Pipelines ────────────────────────────────────────
    if env
        .get("BITBUCKET_BUILD_NUMBER")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        let mut out = VcsMetadata {
            provider: Some("bitbucket"),
            ..Default::default()
        };
        set_if_present(&mut out.commit_sha, env.get("BITBUCKET_COMMIT"));
        // Prefer the full repo name when given; the slug is the shorter
        // form used as a fallback.
        let repo = env
            .get("BITBUCKET_REPO_FULL_NAME")
            .or_else(|| env.get("BITBUCKET_REPO_SLUG"));
        set_if_present(&mut out.repo, repo);
        set_if_present(&mut out.branch, env.get("BITBUCKET_BRANCH"));
        if env
            .get("BITBUCKET_PR_ID")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            set_if_present(
                &mut out.base_branch,
                env.get("BITBUCKET_PR_DESTINATION_BRANCH"),
            );
            if let Some(pr) = env.get("BITBUCKET_PR_ID") {
                if let Ok(n) = pr.trim().parse::<u64>() {
                    out.pr_number = Some(n);
                }
            }
        }
        return out;
    }

    // Git fallback ───────────────────────────────────────────────
    git_fallback(working_dir)
}

/// `working_dir` need not be a git repo — caller gets an empty
/// metadata back. Shells `git` via `env git ...` (not `git`) so a
/// stripped-down CI container that pinned a specific git via PATH
/// still finds it.
fn git_fallback(working_dir: &Path) -> VcsMetadata {
    let mut out = VcsMetadata::default();
    if !working_dir.is_dir() {
        return out;
    }
    // Single `git rev-parse` invocation that prints BOTH the full SHA
    // and the abbrev-ref on separate lines, instead of forking `git`
    // twice. `git rev-parse HEAD --abbrev-ref HEAD` emits two lines:
    //   <40-char SHA>
    //   <branch or "HEAD">
    // Cuts the process-fork cost in half on every build that lacks a
    // CI provider env var (local dev + bare-metal CI).
    let combined = match run_git(
        working_dir,
        &["rev-parse", "HEAD", "--abbrev-ref", "HEAD"],
    ) {
        Some(s) => s,
        None => return out,
    };
    let mut lines = combined.lines();
    if let Some(commit) = lines.next() {
        let commit = commit.trim();
        if !commit.is_empty() {
            out.commit_sha = Some(commit.to_string());
        }
    }
    if let Some(branch) = lines.next() {
        let branch = branch.trim();
        // Detached HEAD surfaces as the literal string "HEAD" — not a
        // meaningful branch name; treat as absent so the back-end
        // doesn't display "HEAD" as the branch.
        if !branch.is_empty() && branch != "HEAD" {
            out.branch = Some(branch.to_string());
        }
    }
    out
}

/// Run `git` with the given args in `working_dir`. Returns the trimmed
/// stdout on success, `None` on any failure (non-zero exit, missing
/// binary, timeout-equivalent kill).
///
/// `git` is intentionally PATH-resolved here (no absolute path), even
/// though the rest of this codebase uses `/usr/bin/<tool>` absolute
/// paths. `git` is user-configurable in a way that `hostname` /
/// `xcodebuild` / `otool` are not — Homebrew users run
/// `/opt/homebrew/bin/git`, asdf/mise users get a shim, NixOS users
/// don't have a system `/usr/bin/git` at all. Pinning the absolute
/// path would silently fall back to "no VCS metadata" on every one
/// of those configurations. The trade-off: in a shared CI workspace
/// where an attacker can write to a directory earlier on PATH than
/// the build user's intended git, a malicious shim runs in the
/// build user's context. Document the threat model rather than
/// pretend it isn't there.
fn run_git(working_dir: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(working_dir)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
}

/// Truthy-token check matching the Bugsee Android Gradle plugin's set
/// (`"1"`, `"true"`, `"yes"`, `"on"`, case-insensitive). One CI config
/// snippet enables a feature on both platforms when the token set
/// matches.
fn env_truthy(value: Option<&String>) -> bool {
    value
        .map(|v| v.trim().to_ascii_lowercase())
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Helper: only write the destination when the source is non-empty
/// after trimming. Keeps the JSON payload tidy — the back-end
/// distinguishes "unknown" from "known empty" by field presence, so
/// an empty string here is the wrong shape.
fn set_if_present(dest: &mut Option<String>, value: Option<&String>) {
    if let Some(v) = value {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            *dest = Some(trimmed.to_string());
        }
    }
}

/// Same as `set_if_present` for raw &str inputs (used for the
/// `strip_prefix` GitHub branch path).
fn set_if_present_raw(dest: &mut Option<String>, value: Option<&str>) {
    if let Some(v) = value {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            *dest = Some(trimmed.to_string());
        }
    }
}

/// Extract the PR number from a GitHub `refs/pull/<n>/<...>` ref form.
/// `<...>` is typically `merge` for pull_request events but may be
/// something else; this only cares about the numeric part.
fn parse_github_pull_ref(refs: &str) -> Option<u64> {
    let rest = refs.strip_prefix("refs/pull/")?;
    let n_str = rest.split('/').next()?;
    n_str.parse::<u64>().ok()
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn env_with(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // ── env_truthy — cross-language contract ─────────────────────

    #[test]
    fn env_truthy_canonical_tokens() {
        for tok in &["1", "true", "yes", "on", "TRUE", "YES", "  on  "] {
            assert!(
                env_truthy(Some(&tok.to_string())),
                "expected truthy: {:?}",
                tok
            );
        }
    }

    #[test]
    fn env_truthy_falsy_tokens() {
        for tok in &["", "   ", "0", "false", "no", "off", "maybe"] {
            assert!(
                !env_truthy(Some(&tok.to_string())),
                "expected falsy: {:?}",
                tok
            );
        }
        assert!(!env_truthy(None));
    }

    // ── GitHub Actions ────────────────────────────────────────────

    #[test]
    fn github_actions_push_event() {
        let env = env_with(&[
            ("GITHUB_ACTIONS", "true"),
            ("GITHUB_SHA", "abc123def456"),
            ("GITHUB_REPOSITORY", "org/repo"),
            ("GITHUB_REF", "refs/heads/master"),
            ("GITHUB_EVENT_NAME", "push"),
        ]);
        let m = resolve(&env, Path::new("/no/such/dir"));
        assert_eq!(m.provider, Some("github"));
        assert_eq!(m.commit_sha.as_deref(), Some("abc123def456"));
        assert_eq!(m.repo.as_deref(), Some("org/repo"));
        assert_eq!(m.branch.as_deref(), Some("master"));
        // Push events must NOT carry pr_number / base_branch — the
        // dashboard treats their presence as "this build is a PR
        // build", which would mis-render every push.
        assert_eq!(m.pr_number, None);
        assert_eq!(m.base_branch, None);
    }

    #[test]
    fn github_actions_tag_push_leaves_branch_absent() {
        // Tag pushes set `GITHUB_REF=refs/tags/<tag>` with
        // `GITHUB_EVENT_NAME=push`. Pre-fix the unwrap-or-raw
        // fallback would emit `branch: "refs/tags/v1.0.0"` and the
        // dashboard would render the literal ref string in the
        // branch column. The Kotlin canonical resolver (Android
        // Gradle plugin) leaves branch absent on tag pushes; this
        // pins the Rust port to the same behaviour.
        let env = env_with(&[
            ("GITHUB_ACTIONS", "true"),
            ("GITHUB_SHA", "tag-sha"),
            ("GITHUB_REPOSITORY", "org/repo"),
            ("GITHUB_REF", "refs/tags/v1.0.0"),
            ("GITHUB_EVENT_NAME", "push"),
        ]);
        let m = resolve(&env, Path::new("/no/such/dir"));
        assert_eq!(m.provider, Some("github"));
        assert_eq!(m.commit_sha.as_deref(), Some("tag-sha"));
        assert_eq!(
            m.branch, None,
            "tag-pushed branch must be None, not the literal ref string",
        );
    }

    #[test]
    fn github_actions_pull_request_event() {
        let env = env_with(&[
            ("GITHUB_ACTIONS", "true"),
            ("GITHUB_SHA", "pr-sha"),
            ("GITHUB_REPOSITORY", "org/repo"),
            ("GITHUB_HEAD_REF", "feature/x"),
            ("GITHUB_BASE_REF", "main"),
            ("GITHUB_REF", "refs/pull/42/merge"),
            ("GITHUB_EVENT_NAME", "pull_request"),
        ]);
        let m = resolve(&env, Path::new("/no/such/dir"));
        assert_eq!(m.provider, Some("github"));
        assert_eq!(m.commit_sha.as_deref(), Some("pr-sha"));
        assert_eq!(m.branch.as_deref(), Some("feature/x"));
        assert_eq!(m.base_branch.as_deref(), Some("main"));
        // pr_number is u64, NOT String — the dashboard's mongo query
        // filters by numeric type and a stringified "42" would be
        // invisible. Pin the type.
        assert_eq!(m.pr_number, Some(42));
    }

    // ── GitLab CI ─────────────────────────────────────────────────

    #[test]
    fn gitlab_merge_request_pipeline() {
        let env = env_with(&[
            ("GITLAB_CI", "true"),
            ("CI_COMMIT_SHA", "gl-sha"),
            ("CI_PROJECT_PATH", "group/project"),
            ("CI_MERGE_REQUEST_IID", "7"),
            ("CI_MERGE_REQUEST_SOURCE_BRANCH_NAME", "feat/foo"),
            ("CI_MERGE_REQUEST_TARGET_BRANCH_NAME", "develop"),
        ]);
        let m = resolve(&env, Path::new("/no/such/dir"));
        assert_eq!(m.provider, Some("gitlab"));
        assert_eq!(m.commit_sha.as_deref(), Some("gl-sha"));
        assert_eq!(m.repo.as_deref(), Some("group/project"));
        assert_eq!(m.branch.as_deref(), Some("feat/foo"));
        assert_eq!(m.base_branch.as_deref(), Some("develop"));
        // IID — NOT ID. ID is a global DB key, useless as a PR
        // reference. Pin the source.
        assert_eq!(m.pr_number, Some(7));
    }

    #[test]
    fn gitlab_push_pipeline() {
        let env = env_with(&[
            ("GITLAB_CI", "true"),
            ("CI_COMMIT_SHA", "gl-push-sha"),
            ("CI_COMMIT_REF_NAME", "master"),
        ]);
        let m = resolve(&env, Path::new("/no/such/dir"));
        // Provider pin — pre-fix this assertion was missing, so a
        // mutation that returned `Some("github")` (wrong provider
        // label) would have slipped through. The dashboard groups
        // builds by provider; a wrong label silently buckets them
        // under the wrong CI system.
        assert_eq!(m.provider, Some("gitlab"));
        assert_eq!(m.commit_sha.as_deref(), Some("gl-push-sha"));
        assert_eq!(m.branch.as_deref(), Some("master"));
        assert_eq!(m.pr_number, None);
        assert_eq!(m.base_branch, None);
    }

    // ── Bitbucket ─────────────────────────────────────────────────

    #[test]
    fn bitbucket_pr_pipeline() {
        let env = env_with(&[
            ("BITBUCKET_BUILD_NUMBER", "42"),
            ("BITBUCKET_COMMIT", "bb-sha"),
            ("BITBUCKET_REPO_FULL_NAME", "team/repo"),
            ("BITBUCKET_BRANCH", "feature/x"),
            ("BITBUCKET_PR_ID", "99"),
            ("BITBUCKET_PR_DESTINATION_BRANCH", "master"),
        ]);
        let m = resolve(&env, Path::new("/no/such/dir"));
        assert_eq!(m.provider, Some("bitbucket"));
        assert_eq!(m.commit_sha.as_deref(), Some("bb-sha"));
        assert_eq!(m.repo.as_deref(), Some("team/repo"));
        assert_eq!(m.branch.as_deref(), Some("feature/x"));
        assert_eq!(m.base_branch.as_deref(), Some("master"));
        assert_eq!(m.pr_number, Some(99));
    }

    // ── Git fallback — real repo ──────────────────────────────────

    #[test]
    fn git_fallback_in_real_repo() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        assert!(Command::new("git")
            .args(&["init", "-q", "-b", "main"])
            .current_dir(repo)
            .status()
            .unwrap()
            .success());
        Command::new("git")
            .args(&["config", "user.email", "test@example.com"])
            .current_dir(repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(&["config", "user.name", "Test"])
            .current_dir(repo)
            .status()
            .unwrap();
        fs::write(repo.join("a.txt"), "hi").unwrap();
        Command::new("git")
            .args(&["add", "."])
            .current_dir(repo)
            .status()
            .unwrap();
        Command::new("git")
            .args(&["commit", "-q", "-m", "init"])
            .current_dir(repo)
            .status()
            .unwrap();

        // No CI env → falls through to git fallback.
        let env = HashMap::new();
        let m = resolve(&env, repo);
        assert!(m.commit_sha.is_some());
        assert_eq!(m.commit_sha.as_ref().unwrap().len(), 40); // full SHA-1
        assert_eq!(m.branch.as_deref(), Some("main"));
        assert_eq!(m.provider, None);
    }

    #[test]
    fn git_fallback_omits_branch_when_head_is_detached() {
        // `git rev-parse --abbrev-ref HEAD` prints the literal string
        // "HEAD" when the repository is in detached-HEAD state
        // (after `git checkout <sha>`, or a CI build that checked out
        // a tag/commit). git_fallback already filters this with
        // `branch != "HEAD"` (line 219); pin the filter so a future
        // refactor can't silently start emitting the literal "HEAD"
        // in the dashboard's branch column.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        for args in [
            ["init", "-q", "-b", "main"].as_slice(),
            ["config", "user.email", "test@example.com"].as_slice(),
            ["config", "user.name", "Test"].as_slice(),
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap()
                .success());
        }
        fs::write(repo.join("a.txt"), "hi").unwrap();
        for args in [
            ["add", "."].as_slice(),
            ["commit", "-q", "-m", "init"].as_slice(),
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap()
                .success());
        }

        // Detach HEAD by checking out the commit by sha.
        let sha_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .unwrap();
        let sha = String::from_utf8(sha_out.stdout).unwrap().trim().to_string();
        assert!(Command::new("git")
            .args(["checkout", "-q", "--detach", &sha])
            .current_dir(repo)
            .status()
            .unwrap()
            .success());

        let env = HashMap::new();
        let m = resolve(&env, repo);
        // Commit sha still present.
        assert_eq!(m.commit_sha.as_ref().unwrap().len(), 40);
        // Branch must be None — NOT the literal "HEAD".
        assert_eq!(
            m.branch, None,
            "detached HEAD must produce branch=None, not literal HEAD",
        );
    }

    #[test]
    fn git_fallback_empty_for_nonexistent_dir() {
        let env = HashMap::new();
        let m = resolve(&env, Path::new("/no/such/directory"));
        assert_eq!(m, VcsMetadata::default());
    }

    #[test]
    fn git_fallback_empty_for_non_git_dir() {
        let tmp = TempDir::new().unwrap();
        let env = HashMap::new();
        let m = resolve(&env, tmp.path());
        assert_eq!(m, VcsMetadata::default());
    }

    // ── JSON wire shape ───────────────────────────────────────────

    #[test]
    fn json_omits_absent_fields() {
        // The back-end's `sanitizeVcs` distinguishes "unknown" from
        // "known empty" by field presence. A serialised
        // `"provider": null` would silently fail that check on
        // either side. Pin that absent fields are OMITTED.
        let m = VcsMetadata::default();
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn json_round_trips_through_serde_for_a_typical_payload() {
        let env = env_with(&[
            ("GITHUB_ACTIONS", "true"),
            ("GITHUB_SHA", "abc"),
            ("GITHUB_REPOSITORY", "org/repo"),
            ("GITHUB_REF", "refs/heads/main"),
            ("GITHUB_EVENT_NAME", "push"),
        ]);
        let m = resolve(&env, Path::new("/no/such/dir"));
        let json = serde_json::to_string(&m).unwrap();
        // Field names + the type of pr_number are the load-bearing
        // contract — pin both by string-matching the output.
        assert!(json.contains("\"provider\":\"github\""));
        assert!(json.contains("\"commit_sha\":\"abc\""));
        assert!(json.contains("\"repo\":\"org/repo\""));
        assert!(json.contains("\"branch\":\"main\""));
        // Absent fields must NOT appear as `null` — Option + skip
        // serialisation. Catches a future regression that flipped
        // `skip_serializing_if`.
        assert!(!json.contains("null"));
        assert!(!json.contains("pr_number"));
    }

    // ── parse_github_pull_ref ─────────────────────────────────────

    #[test]
    fn parse_github_pull_ref_canonical_form() {
        assert_eq!(parse_github_pull_ref("refs/pull/42/merge"), Some(42));
        assert_eq!(parse_github_pull_ref("refs/pull/9999/head"), Some(9999));
    }

    #[test]
    fn parse_github_pull_ref_rejects_non_pull_refs() {
        assert_eq!(parse_github_pull_ref("refs/heads/main"), None);
        assert_eq!(parse_github_pull_ref(""), None);
        assert_eq!(parse_github_pull_ref("refs/pull/notanumber/merge"), None);
    }

    // ── set_if_present — empty-string discipline ──────────────────

    #[test]
    fn set_if_present_skips_empty_and_whitespace() {
        let mut dest: Option<String> = None;
        set_if_present(&mut dest, Some(&"".to_string()));
        set_if_present(&mut dest, Some(&"   ".to_string()));
        assert_eq!(dest, None);
        set_if_present(&mut dest, Some(&"value".to_string()));
        assert_eq!(dest.as_deref(), Some("value"));
        set_if_present(&mut dest, Some(&"  trimmed  ".to_string()));
        assert_eq!(dest.as_deref(), Some("trimmed"));
    }
}
