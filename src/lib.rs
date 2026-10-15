//! harness — an AI agent harness for code creation and project updates.
//!
//! A headless agent loop drives an OpenAI-compatible backend to work on a
//! git-tracked workspace, with per-tool approval, append-only JSONL session
//! logs, automatic context compaction, and a TUI on top.

pub mod agent;
pub mod backend;
pub mod config;
pub mod logmsg;
pub mod state;
pub mod tools;
pub mod tui;
