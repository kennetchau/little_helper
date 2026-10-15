//! Git helpers shared by seeding and per-turn commits.
//!
//! Failures print a warning to stderr and return a falsy value; they never
//! panic, so a git hiccup can't kill a session. Failure details go through
//! `logmsg` (conversation line in TUI mode) and are rate-limited to ONE
//! warning per process: a broken repo would otherwise be re-announced on
//! every turn's commit — and in TUI mode every stray stderr line shows up
//! as garbage over the UI. A missing repo is self-healed (re-init) on the
//! next `commit_all` instead of failing forever.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::logmsg;

/// Failure details are printed at most once per process — a persistent
/// problem (e.g. a wiped repo) must not spam the log/conversation.
static GIT_WARNED: AtomicBool = AtomicBool::new(false);

fn git_fail(args: &[&str], cwd: &Path, detail: &str) {
    if GIT_WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    logmsg::warn(format!(
        "[git] {} failed in {}: {}",
        args.join(" "),
        cwd.display(),
        detail
    ));
}

/// Run a git command. Warns (once per process) on failure. Returns true on success.
pub fn run_git(args: &[&str], cwd: &Path) -> bool {
    let out = match Command::new("git").args(args).current_dir(cwd).output() {
        Ok(out) => out,
        Err(e) => {
            git_fail(args, cwd, &format!("spawn: {e}"));
            return false;
        }
    };
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr)
            .trim()
            .to_string();
        let detail = if detail.is_empty() {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            detail
        };
        git_fail(args, cwd, &detail);
        return false;
    }
    true
}

fn git_stdout(args: &[&str], cwd: &Path) -> String {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// `git init` + identity config (idempotent precondition for committing).
fn init_repo(path: &Path) -> bool {
    run_git(&["init"], path)
        && run_git(&["config", "user.name", "harness"], path)
        && run_git(&["config", "user.email", "harness@localhost"], path)
}

/// Create dir, git init if needed, initial commit if there is content.
pub fn ensure_repo(path: &Path, message: &str) {
    if let Err(e) = std::fs::create_dir_all(path) {
        git_fail(&[], path, &format!("cannot create {path:?}: {e}"));
        return;
    }
    if !path.join(".git").exists() {
        init_repo(path);
    }
    commit_all(path, message);
}

/// `git add -A` + commit. Returns the short hash, or None when nothing changed.
pub fn commit_all(path: &Path, message: &str) -> Option<String> {
    if !path.join(".git").exists() {
        // Self-heal: the repo is gone (init failed at seed time, the mount
        // was wiped, ...) — re-initialize instead of failing every turn
        // with "not a git repository (… filesystem boundary …)". Init only:
        // the commit below runs with the CALLER's message.
        init_repo(path);
        if !path.join(".git").exists() {
            // git unavailable: stay quiet, never spam, never panic.
            return None;
        }
    }
    if !run_git(&["add", "-A"], path) {
        return None;
    }
    if git_stdout(&["status", "--porcelain"], path).is_empty() {
        return None;
    }
    if !run_git(&["commit", "-m", message], path) {
        return None;
    }
    let hash = git_stdout(&["rev-parse", "--short", "HEAD"], path);
    (!hash.is_empty()).then_some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "harness-git-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn commit_all_reinitializes_a_missing_repo() {
        let dir = tmp("reinit");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        // no .git yet: commit_all must init the repo and commit (root
        // cause of the per-turn "not a git repository" stderr spam)
        let hash = commit_all(&dir, "test: reinit");
        assert!(hash.is_some(), "expected a commit hash, got None");
        assert!(dir.join(".git").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_all_clean_tree_returns_none() {
        let dir = tmp("clean");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        commit_all(&dir, "test: first").unwrap();
        assert_eq!(commit_all(&dir, "test: noop"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
