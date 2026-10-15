//! TUI-safe runtime logging.
//!
//! While the TUI owns the alternate screen, anything written straight to
//! stderr/stdout (a stray `eprintln!`) is painted over the rendered UI at
//! the terminal's cursor position — that is the "weird text" garbage users
//! saw when git failed mid-turn. Every runtime warning (git, session,
//! config, backend) therefore goes through [`warn`] / [`log`] instead of
//! raw `eprintln!`/`println!`:
//!
//! - TUI mode: `tui::run_inner` registers a sink; messages are forwarded
//!   into the conversation as styled lines.
//! - headless / startup (no sink yet): they fall back to the plain
//!   terminal exactly as before.

use std::sync::Mutex;

use tokio::sync::mpsc::UnboundedSender;

static SINK: Mutex<Option<UnboundedSender<String>>> = Mutex::new(None);

/// Register (or clear, with `None`) the TUI sink.
pub fn set_sink(tx: Option<UnboundedSender<String>>) {
    *SINK.lock().unwrap_or_else(|e| e.into_inner()) = tx;
}

/// A warning: rendered in the conversation in TUI mode, otherwise
/// `eprintln!` (headless behavior).
pub fn warn(msg: String) {
    if let Some(tx) = SINK.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = tx.send(msg);
        return;
    }
    eprintln!("{msg}");
}

/// Informational runtime note (e.g. "[seed] wrote default config"):
/// conversation in TUI mode, otherwise `println!`.
pub fn log(msg: String) {
    if let Some(tx) = SINK.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = tx.send(msg);
        return;
    }
    println!("{msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    // The SINK is process-global, so these tests must not run in parallel
    // (they each register/clear it) — same pattern as tests/seed.rs.
    static SINK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn warn_and_log_route_to_registered_sink() {
        let _g = SINK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        set_sink(Some(tx));
        warn("git failed once".into());
        log("seed note".into());
        set_sink(None);
        assert_eq!(rx.recv().await.as_deref(), Some("git failed once"));
        assert_eq!(rx.recv().await.as_deref(), Some("seed note"));
    }

    #[tokio::test]
    async fn warn_is_dropped_when_sink_is_gone() {
        let _g = SINK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // sender dropped (UI task exited) -> no panic, message dropped
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        set_sink(Some(tx));
        warn("no listener".into());
        set_sink(None);
    }
}
