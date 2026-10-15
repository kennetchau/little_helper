//! First-run seeding of the state dir and the workspace repo. Idempotent.

use std::path::Path;

use crate::config::{self, Config, DEFAULT_CONFIG_TEMPLATE};
use crate::logmsg;
use crate::state::gitstore::ensure_repo;

/// Seed the state dir and the workspace. Returns true if config.toml was
/// created (caller should reload the config in that case).
pub fn seed(cfg: &Config) -> bool {
    let state = config::state_dir();
    if let Err(e) = std::fs::create_dir_all(&state) {
        logmsg::warn(format!("[seed] cannot create state dir {state:?}: {e}"));
        return false;
    }
    let config_file = config::config_path();
    let created = !config_file.exists();
    if created {
        match std::fs::write(&config_file, DEFAULT_CONFIG_TEMPLATE) {
            Ok(()) => logmsg::log(format!("[seed] wrote default config: {:?}", config_file)),
            Err(e) => logmsg::warn(format!("[seed] cannot write config: {e}")),
        }
    }
    for sub in ["agents", "sessions"] {
        if let Err(e) = std::fs::create_dir_all(state.join(sub)) {
            logmsg::warn(format!("[seed] cannot create {sub} dir: {e}"));
        }
    }
    ensure_repo(&state, "harness: initial state");
    if cfg.workspace.auto_git {
        ensure_repo(Path::new(&cfg.workspace.path), "harness: initial import");
    }
    created
}
