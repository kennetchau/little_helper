//! Phase-1 seed test: ephemeral state dir + workspace, idempotent, git repos.
//!
//! `seed()` reads `$HARNESS_STATE` (like the Python `STATE_DIR`), so the seed
//! test runs under a process-global env lock to avoid racing other tests.

use std::sync::{Mutex, MutexGuard};

use harness::config::{self, Config};
use harness::state::seed;

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn tmp(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("harness-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

#[test]
fn seed_creates_state_and_repos_idempotently() {
    let _lock = env_lock();
    let state = tmp("state");
    let ws = tmp("ws");
    // SAFETY: held `env_lock` guarantees no other test reads/writes env vars
    // concurrently in this (per-binary) test process.
    unsafe { std::env::set_var("HARNESS_STATE", &state) };

    let cfg = Config {
        workspace: config::WorkspaceConfig {
            path: ws.to_string_lossy().into(),
            auto_git: true,
        },
        ..Config::default()
    };

    let created1 = seed::seed(&cfg);
    assert!(created1, "first seed must create config.toml");
    assert!(config::config_path().exists());
    assert!(state.join("agents").is_dir());
    assert!(state.join("sessions").is_dir());
    assert!(state.join(".git").exists());
    assert!(ws.join(".git").exists());

    // Second run: idempotent, no config rewrite.
    let created2 = seed::seed(&cfg);
    assert!(!created2, "second seed must not recreate config");

    // config.toml must be tracked in the state repo (the Python contract).
    let tracked = std::process::Command::new("git")
        .args(["ls-files", "config.toml"])
        .current_dir(&state)
        .output()
        .expect("git ls-files")
        .stdout;
    assert!(
        String::from_utf8_lossy(&tracked).contains("config.toml"),
        "config.toml must be git-tracked"
    );

    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn gitstore_commit_all_returns_hash_then_none() {
    let dir = tmp("git");
    let _ = std::fs::create_dir_all(&dir);
    harness::state::gitstore::ensure_repo(&dir, "init");
    std::fs::write(dir.join("a.txt"), "hello").unwrap();

    let h1 = harness::state::gitstore::commit_all(&dir, "add a");
    assert!(h1.is_some(), "first commit should have a hash");
    let h2 = harness::state::gitstore::commit_all(&dir, "nothing changed");
    assert!(h2.is_none(), "no changes -> None");
    let _ = std::fs::remove_dir_all(&dir);
}
