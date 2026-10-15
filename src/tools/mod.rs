//! Tool registry + file/shell/web tools.

pub mod files;
pub mod registry;
pub mod shell;
pub mod web;

pub use registry::*;

static REGISTERED: std::sync::Once = std::sync::Once::new();

/// Register every built-in tool. Idempotent — safe to call from several
/// entry points (main, tests).
pub fn register_all() {
    REGISTERED.call_once(|| {
        files::register_all();
        shell::register_all();
        web::register_all();
    });
}
