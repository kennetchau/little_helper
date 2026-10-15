//! Append-only JSONL session store + replay.
//!
//! One file per session under `$HARNESS_STATE/sessions/<id>.jsonl`. Events
//! are the audit log (git-tracked in /state); `replay()` rebuilds the
//! working memory the model sees. Contract (unchanged from Python — Rust
//! and Python sessions must be resumable by each other):
//!
//! ```text
//! memory = system message
//!        + all summary texts, in order
//!        + all msg/tool events after the LAST summary
//! ```
//!
//! Everything before the last summary is intentionally dropped from memory
//! (it lives on in the JSONL, which is what you audit). Only `msg` and
//! `tool` events become wire messages; everything else (approval,
//! turn_end, error, interrupted, agent, prompt, ...) is metadata skipped
//! by replay — except `prompt`, which overrides the system message (the
//! phase-5 `update_system_prompt` feature).
//!
//! This module never raises on I/O or corrupt lines: a bad line is skipped
//! so a hiccup can't kill a session.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config;

/// Marker for a compacted span, per requirements. The loop and replay both
/// build the summary user message through [`summary_message`] so they can't
/// drift apart.
pub const SUMMARY_MARKER: &str = "[Summary of prior conversation]";

fn sessions_dir() -> PathBuf {
    config::state_dir().join("sessions")
}

/// `YYYYMMDD-HHMM-<agent>`, local time. e.g. `20260709-1430-coder`.
/// Mirrors Python `time.strftime("%Y%m%d-%H%M", time.localtime(now))`.
pub fn new_session_id(agent: &str, now: Option<i64>) -> String {
    let ts = now.unwrap_or_else(|| chrono::Local::now().timestamp());
    let dt = chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&chrono::Local))
        .expect("valid timestamp");
    format!("{}-{agent}", dt.format("%Y%m%d-%H%M"))
}

/// The single user message a compacted span is replaced by.
pub fn summary_message(text: &str) -> Value {
    json!({ "role": "user", "content": format!("{SUMMARY_MARKER}\n{text}") })
}

/// Map a msg/tool event to an OpenAI wire message; None otherwise.
fn event_to_message(ev: &Value) -> Option<Value> {
    let t = ev.get("type")?.as_str()?;
    match t {
        "msg" => {
            let mut msg = json!({
                "role": ev.get("role").cloned().unwrap_or(Value::Null),
                "content": ev.get("content").and_then(Value::as_str).unwrap_or(""),
            });
            if let Some(tc) = ev.get("tool_calls") {
                msg["tool_calls"] = tc.clone();
            }
            if let Some(r) = ev.get("reasoning_content").and_then(Value::as_str) {
                if !r.is_empty() {
                    msg["reasoning_content"] = Value::String(r.to_string());
                }
            }
            Some(msg)
        }
        "tool" => {
            // name/args/ok are audit-only; the wire format needs id + result.
            Some(json!({
                "role": "tool",
                "tool_call_id": ev.get("id").cloned().unwrap_or(Value::Null),
                "content": ev.get("result").and_then(Value::as_str).unwrap_or(""),
            }))
        }
        _ => None,
    }
}

/// Read every parseable event. Missing file -> []; bad line -> skipped.
fn read_all(path: &Path) -> Vec<Value> {
    if !path.exists() {
        return Vec::new();
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// A single session: an append-only event log plus derived memory.
pub struct Session {
    pub path: PathBuf,
    agent0: String,
    pub model: String,
    events: Vec<Value>,
}

impl Session {
    // ---------- construction ----------

    /// New session file under `sessions_dir`; same-minute collisions get
    /// suffixes `-1`, `-2`, ... Appends the initial `session` event.
    pub fn create(agent: &str, model: &str, dir: Option<&Path>) -> Session {
        let dir = match dir {
            Some(d) => d.to_path_buf(),
            None => sessions_dir(),
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            crate::logmsg::warn(format!("[session] cannot create {dir:?}: {e}"));
        }
        let base = new_session_id(agent, None);
        let mut path = dir.join(format!("{base}.jsonl"));
        let mut n = 1;
        while path.exists() {
            path = dir.join(format!("{base}-{n}.jsonl"));
            n += 1;
        }
        let mut s = Session {
            path,
            agent0: agent.to_string(),
            model: model.to_string(),
            events: Vec::new(),
        };
        s.append(json!({
            "type": "session",
            "id": s.id(),
            "agent": agent,
            "model": model,
        }));
        s
    }

    /// Resume an existing session file, recovering agent/model from it.
    pub fn open(path: &Path) -> Session {
        let events = read_all(path);
        let (mut agent, mut model) = (String::from("coder"), String::new());
        for ev in &events {
            if ev.get("type").and_then(Value::as_str) == Some("session") {
                if let Some(a) = ev.get("agent").and_then(Value::as_str) {
                    agent = a.to_string();
                }
                if let Some(m) = ev.get("model").and_then(Value::as_str) {
                    model = m.to_string();
                }
                break;
            }
        }
        Session {
            path: path.to_path_buf(),
            agent0: agent,
            model,
            events,
        }
    }

    // ---------- identity / derived state ----------

    pub fn id(&self) -> String {
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Current agent: latest `agent` event, else the session's agent.
    pub fn agent(&self) -> String {
        let mut name = self.agent0.clone();
        for ev in &self.events {
            if ev.get("type").and_then(Value::as_str) == Some("agent") {
                if let Some(n) = ev.get("name").and_then(Value::as_str) {
                    name = n.to_string();
                }
            }
        }
        name
    }

    /// Completed turns (last turn_end.n, 0 for a fresh session).
    pub fn turn_number(&self) -> u64 {
        let mut n = 0u64;
        for ev in &self.events {
            if ev.get("type").and_then(Value::as_str) == Some("turn_end") {
                n = ev.get("n").and_then(Value::as_u64).unwrap_or(n);
            }
        }
        n
    }

    /// Last known prompt_tokens (last turn_end), 0 if none yet.
    pub fn last_prompt_tokens(&self) -> u64 {
        let mut tokens = 0u64;
        for ev in &self.events {
            if ev.get("type").and_then(Value::as_str) == Some("turn_end") {
                tokens = ev
                    .get("prompt_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(tokens);
            }
        }
        tokens
    }

    /// Last commit (last turn_end), None if none yet.
    pub fn last_commit(&self) -> Option<String> {
        let mut commit: Option<String> = None;
        for ev in &self.events {
            if ev.get("type").and_then(Value::as_str) == Some("turn_end") {
                commit = ev
                    .get("commit")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
        commit
    }

    /// Last recorded system prompt (phase-5 `prompt` events); None when the
    /// session has none -> callers fall back to the composed default.
    pub fn last_prompt(&self) -> Option<String> {
        self.events
            .iter()
            .rev()
            .find(|ev| ev.get("type").and_then(Value::as_str) == Some("prompt"))
            .and_then(|ev| ev.get("text").and_then(Value::as_str))
            .map(str::to_string)
    }

    /// All events so far (audit log in memory). The loop's backfill pass
    /// scans this for already-recorded tool ids (Python: `session._events`).
    pub fn events(&self) -> &[Value] {
        &self.events
    }

    // ---------- persistence ----------

    /// Append one event to memory and to the JSONL file. Never panics on
    /// I/O (a failed write warns; memory keeps advancing like Python's
    // in-memory list — note: Python appends memory first too).
    pub fn append(&mut self, event: Value) {
        self.events.push(event.clone());
        let line = serde_json::to_string(&event).unwrap_or_else(|e| {
            crate::logmsg::warn(format!("[session] cannot serialize event: {e}"));
            Value::String(String::new()).to_string()
        });
        let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            crate::logmsg::warn(format!("[session] cannot open {:?} for append", self.path));
            return;
        };
        use std::io::Write;
        if let Err(e) = writeln!(f, "{line}") {
            crate::logmsg::warn(format!("[session] append to {:?} failed: {e}", self.path));
        }
    }

    // ---------- typed event helpers (keep the loop clean) ----------

    pub fn record_user(&mut self, content: &str) {
        self.append(json!({ "type": "msg", "role": "user", "content": content }));
    }

    pub fn record_assistant(
        &mut self,
        content: &Value,
        tool_calls: Option<&Value>,
        reasoning: Option<&str>,
    ) {
        let mut ev = json!({ "type": "msg", "role": "assistant", "content": content });
        if let Some(tc) = tool_calls {
            ev["tool_calls"] = tc.clone();
        }
        // Reasoning is persisted so a resume replays it back to the model
        // (llama.cpp accepts reasoning_content on assistant messages).
        if let Some(r) = reasoning {
            if !r.is_empty() {
                ev["reasoning_content"] = Value::String(r.to_string());
            }
        }
        self.append(ev);
    }

    pub fn record_tool(
        &mut self,
        id: &str,
        name: &str,
        args: &Value,
        result: &str,
        ok: bool,
    ) {
        self.append(json!({
            "type": "tool",
            "id": id,
            "name": name,
            "args": args,
            "result": result,
            "ok": ok,
        }));
    }

    pub fn record_approval(&mut self, tool: &str, decision: &str) {
        self.append(json!({
            "type": "approval",
            "tool": tool,
            "decision": decision,
        }));
    }

    pub fn record_summary(&mut self, text: &str) {
        self.append(json!({ "type": "summary", "text": text }));
    }

    pub fn record_agent(&mut self, name: &str) {
        self.append(json!({ "type": "agent", "name": name }));
    }

    pub fn record_interrupted(&mut self) {
        self.append(json!({ "type": "interrupted" }));
    }

    pub fn record_error(&mut self, text: &str) {
        self.append(json!({ "type": "error", "text": text }));
    }

    pub fn record_turn_aborted(&mut self, reason: &str) {
        self.append(json!({ "type": "turn_aborted", "reason": reason }));
    }

    /// Phase-5: a new system prompt replaces memory[0]; replay picks the
    /// last `prompt` event as the system message.
    pub fn record_prompt(&mut self, text: &str) {
        self.append(json!({ "type": "prompt", "text": text }));
    }

    pub fn record_turn_end(
        &mut self,
        n: u64,
        commit: Option<&str>,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) {
        self.append(json!({
            "type": "turn_end",
            "n": n,
            "commit": commit,
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
        }));
    }

    // ---------- memory ----------

    /// Rebuild working memory per the replay rule (see module docstring).
    /// The system message is the last `prompt` event if present, else
    /// `system_prompt` (composed default). Old sessions without `prompt`
    /// events replay exactly as before.
    pub fn replay(&self, system_prompt: &str) -> Vec<Value> {
        let system = self.last_prompt().unwrap_or_else(|| system_prompt.to_string());
        let mut memory: Vec<Value> = vec![json!({
            "role": "system",
            "content": system,
        })];

        let mut last_summary_idx: Option<usize> = None;
        for (i, ev) in self.events.iter().enumerate() {
            if ev.get("type").and_then(Value::as_str) == Some("summary") {
                last_summary_idx = Some(i);
            }
        }

        // All summaries, in order.
        for ev in &self.events {
            if ev.get("type").and_then(Value::as_str) == Some("summary") {
                memory.push(summary_message(
                    ev.get("text").and_then(Value::as_str).unwrap_or(""),
                ));
            }
        }

        // Post-last-summary msg/tool events only.
        if let Some(idx) = last_summary_idx {
            for ev in &self.events[idx + 1..] {
                if let Some(msg) = event_to_message(ev) {
                    memory.push(msg);
                }
            }
        } else {
            for ev in &self.events {
                if let Some(msg) = event_to_message(ev) {
                    memory.push(msg);
                }
            }
        }

        memory
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_sessions(name: &str) -> (PathBuf, Session) {
        let dir =
            std::env::temp_dir().join(format!("harness-sess-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = Session::create("coder", "m1", Some(&dir));
        (dir, s)
    }

    #[test]
    fn session_id_format() {
        // 2021-01-02 03:04:05 UTC => 1609614245 (wall clock is local; only
        // the shape is asserted, matching Python's local-time behavior)
        let id = new_session_id("coder", Some(1609614245));
        assert!(id.ends_with("-coder"), "{id}");
        assert_eq!(id.len(), "YYYYMMDD-HHMM-coder".len());
        let re = regex::Regex::new(r"^\d{8}-\d{4}-coder$").unwrap();
        assert!(re.is_match(&id), "{id}");
        // monotonic-ish: a fixed ts yields a stable id
        assert_eq!(id, new_session_id("coder", Some(1609614245)));
    }

    #[test]
    fn create_is_unique_and_appends_session_event() {
        let (dir, s) = tmp_sessions("create");
        assert!(s.path.exists());
        // second create same minute -> suffix -1
        let s2 = Session::create("coder", "m1", Some(&dir));
        assert_ne!(s2.path, s.path);
        let content = std::fs::read_to_string(&s.path).unwrap();
        let first: Value =
            serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(first["type"], "session");
        assert_eq!(first["agent"], "coder");
        assert_eq!(first["model"], "m1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_recovers_agent_model_and_skips_corrupt() {
        let (dir, mut s) = tmp_sessions("open");
        s.record_user("hi");
        // append a corrupt line
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&s.path)
                .unwrap();
            writeln!(f, "{{ not json").unwrap();
        }
        s.record_user("bye");
        let reopened = Session::open(&s.path);
        assert_eq!(reopened.agent0, "coder");
        assert_eq!(reopened.model, "m1");
        assert_eq!(reopened.id(), s.id());
        // memory: 2 user msgs (corrupt line skipped), 1 system
        let mem = reopened.replay("SYS");
        let users: Vec<_> = mem
            .iter()
            .filter(|m| m["role"] == "user" && m["type"].is_null())
            .collect();
        assert_eq!(users.len(), 2, "{mem:?}");
        assert_eq!(mem.len(), 3, "{mem:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_rule_and_agent_rename() {
        let (_dir, mut s) = tmp_sessions("replay");
        s.record_user("q1");
        s.record_assistant(&json!("a1"), None, None);
        s.record_summary("SUMMARY1");
        s.record_user("q2");
        s.record_assistant(&json!("a2"), None, None);
        s.record_summary("SUMMARY2");
        s.record_user("q3");
        s.record_agent("writer");
        s.record_turn_end(1, Some("abc123"), 100, 50);
        assert_eq!(s.agent(), "writer");
        assert_eq!(s.turn_number(), 1);
        assert_eq!(s.last_prompt_tokens(), 100);
        assert_eq!(s.last_commit().as_deref(), Some("abc123"));

        let mem = s.replay("SYS");
        let roles: Vec<&str> = mem.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["system", "user", "user", "user"], "{mem:?}");
        // system first, then both summaries, then only q3
        assert_eq!(mem[0]["content"], "SYS");
        assert!(mem[1]["content"]
            .as_str()
            .unwrap()
            .starts_with(SUMMARY_MARKER));
        assert!(mem[1]["content"].as_str().unwrap().contains("SUMMARY1"));
        assert!(mem[2]["content"].as_str().unwrap().contains("SUMMARY2"));
        assert_eq!(mem[3]["content"], "q3");
        // turn_end/agent/approval must NOT leak into memory
        assert!(mem.iter().all(|m| !m.get("type").is_some()));
    }

    #[test]
    fn tool_events_replay_to_wire_shape() {
        let (_dir, mut s) = tmp_sessions("tool");
        s.record_user("q");
        s.record_assistant(
            &json!(""),
            Some(&json!([{"id": "call_1", "type": "function",
                          "function": {"name": "list_dir", "arguments": "{}"}}])),
            None,
        );
        s.record_tool("call_1", "list_dir", &json!({}), "file.txt", true);
        s.record_approval("list_dir", "auto");
        let mem = s.replay("SYS");
        // system, user q, assistant(tool_calls), tool result
        assert_eq!(mem.len(), 4, "{mem:?}");
        assert_eq!(mem[1]["role"], "user");
        assert_eq!(mem[2]["role"], "assistant");
        assert_eq!(mem[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(mem[3]["role"], "tool");
        assert_eq!(mem[3]["tool_call_id"], "call_1");
        assert_eq!(mem[3]["content"], "file.txt");
    }

    #[test]
    fn prompt_event_overrides_system_on_resume() {
        let (dir, mut s) = tmp_sessions("prompt");
        s.record_user("q");
        s.record_prompt("CUSTOM SYSTEM");
        let mem = s.replay("DEFAULT");
        assert_eq!(mem[0]["content"], "CUSTOM SYSTEM");
        // resume from disk
        let reopened = Session::open(&s.path);
        let mem = reopened.replay("DEFAULT");
        assert_eq!(mem[0]["content"], "CUSTOM SYSTEM");
        // last prompt wins
        s.record_prompt("CUSTOMER");
        assert_eq!(s.replay("DEFAULT")[0]["content"], "CUSTOMER");
        // no prompt event -> default
        let fresh = Session::create("coder", "m", Some(&dir));
        assert_eq!(fresh.replay("DEFAULT")[0]["content"], "DEFAULT");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
