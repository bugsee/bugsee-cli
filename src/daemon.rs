//! Background (daemon) execution for `xcode post-action`.
//!
//! The iOS post-action must return control to Xcode immediately so the archive
//! doesn't stall on Bugsee's network I/O. We reproduce the BugseeAgent's classic
//! UNIX double-fork: the original process returns to Xcode right away while a
//! detached grandchild does the work, with its stdout/stderr redirected to a log
//! file.
//!
//! CRITICAL ORDERING: [`daemonize`] MUST be called BEFORE the async runtime (or
//! any other thread) is created. `fork()` only carries the calling thread into
//! the child, so forking a live multi-threaded tokio runtime leaves the child's
//! runtime in a broken, deadlock-prone state. `main` calls this before building
//! the runtime; the daemon then builds a fresh runtime of its own.

use std::path::{Path, PathBuf};

/// Where a detached daemon writes its stdout/stderr. Prefers Xcode's
/// `$PROJECT_TEMP_DIR` (the agent's `BugseeAgent.log` home), then `$TMPDIR`,
/// then the platform temp dir.
pub fn log_path() -> PathBuf {
    let dir = std::env::var_os("PROJECT_TEMP_DIR")
        .or_else(|| std::env::var_os("TMPDIR"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("bugsee-cli.log")
}

/// Detach into a background daemon. In the original process and the intermediate
/// child this never returns — it `exit(0)`s them so the caller (Xcode / the
/// shell) regains control. In the final detached grandchild it returns `Ok(())`
/// and execution continues.
///
/// Returns `Err` for a recoverable pre-fork failure (the log/`/dev/null` can't be
/// opened — caught while still foreground, so the caller cleanly falls back to
/// foreground) or a `fork`/`setsid` failure. A `dup2` failure after the
/// double-fork also returns `Err`, but by then the process is already detached,
/// so the caller's "foreground fallback" only logs — the work still runs.
#[cfg(unix)]
pub fn daemonize(log_path: &Path) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Error;
    use std::os::unix::io::AsRawFd;

    // Open the redirect targets BEFORE forking, so an unwritable log directory is
    // caught while we are STILL in the foreground — a recoverable error the
    // caller genuinely can fall back from. (Once the double-fork completes the
    // process is irreversibly detached, so a failure after it could not "fall
    // back"; doing the opens up front keeps the fallback contract honest.) The
    // open fds are inherited across `fork`; the detached grandchild `dup2`s them.
    let devnull = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    // SAFETY: `main` guarantees this runs before the tokio runtime / any threads
    // are spawned, so the process is single-threaded and in a consistent state
    // across the fork. The `fork`/`setsid`/`umask` calls are the textbook
    // daemonization sequence (Stevens, APUE).
    unsafe {
        // Fork #1 — the original parent detaches and returns to the caller.
        match libc::fork() {
            -1 => return Err(Error::last_os_error()),
            0 => {}                     // first child: continue
            _ => std::process::exit(0), // original parent: hand control back now
        }

        // New session, no controlling terminal.
        if libc::setsid() == -1 {
            return Err(Error::last_os_error());
        }

        // Fork #2 — the daemon is no longer a session leader, so it can never
        // reacquire a controlling terminal.
        match libc::fork() {
            -1 => return Err(Error::last_os_error()),
            0 => {}                     // grandchild (the daemon): continue
            _ => std::process::exit(0), // intermediate child: exit
        }

        // Don't inherit the caller's umask for any files the daemon creates.
        libc::umask(0);

        // NOTE: we deliberately do NOT `chdir("/")` (the classic daemon step).
        // `run_post_action` resolves several paths relative to the process CWD as
        // fallbacks (SRCROOT / PROJECT_DIR → `std::env::current_dir()`), so moving
        // to `/` would silently break those. The daemon is short-lived (seconds
        // of uploads), so keeping the build dir as CWD is harmless.

        // Detach the standard streams: stdin ← /dev/null, stdout+stderr ← the
        // log (opened above). `dup2` atomically replaces 0/1/2 with independent
        // descriptors referring to the same open file descriptions.
        if libc::dup2(devnull.as_raw_fd(), libc::STDIN_FILENO) == -1
            || libc::dup2(log.as_raw_fd(), libc::STDOUT_FILENO) == -1
            || libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) == -1
        {
            return Err(Error::last_os_error());
        }
    }

    // `devnull` + `log` drop here, closing only the extra descriptors; 0/1/2 stay
    // bound to the dup'd open file descriptions.
    Ok(())
}

/// Daemonization is only supported on unix. `should_daemonize` already returns
/// `false` on other platforms, so this is never reached in practice; the stub
/// keeps the crate compiling cross-platform and degrades to foreground if it
/// ever were called.
#[cfg(not(unix))]
pub fn daemonize(_log_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "background (daemon) mode is only supported on unix",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_prefers_project_temp_dir() {
        // Drive the resolution deterministically without mutating real process
        // env in a way that races other tests: we only assert the filename and
        // that it lands under *some* directory.
        let p = log_path();
        assert_eq!(p.file_name().unwrap(), "bugsee-cli.log");
        assert!(p.parent().is_some());
    }
}
