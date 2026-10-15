//! Context compaction: summarize old turns to free context space.
//!
//! Faithful port of `reference/harness/src/harness/agent/compaction.py`
//! (plan §7 phase 4; §8: do not "simplify" the cut logic).
//!
//! Design note (why summaries are never re-summarized):
//!   Each compaction drops a contiguous block of REAL turns that sits right
//!   after the leading run of summary messages, and inserts ONE new summary
//!   message there. Existing summaries are preserved, so every summary in
//!   memory covers a disjoint chunk of turns and there is at most a growing
//!   list of them at the front. This keeps the live memory exactly consistent
//!   with `Session::replay()`, whose rule is "system + ALL summaries in
//!   order + all msg/tool events after the LAST summary". If we folded old
//!   summaries into a new one, replay would double-count them.
//!
//! The cut always lands either on a real user-message turn start or on a
//! round's assistant message (a round is that message plus its tool results),
//! so an assistant + tool_calls + tool group is never split (chat templates
//! break if those are separated).
//!
//! When a single long tool-call chain outgrows the budget and there are no
//! older user turns to drop, the cut falls inside the current turn:
//! everything before the last `keep_recent_turns` complete rounds — older
//! turns included, and the turn's own user message — is folded into one
//! summary. The kept rounds are pure assistant/tool groups, which is the
//! same shape memory already has mid-turn, so replay stays consistent.
//!
//! Deviation from Python (documented per AGENT.md): Python's tests
//! monkeypatch the module-global `_find_cut`. Rust has no monkeypatching,
//! so the cut strategy is an injectable value ([`FindCut`]) instead of a
//! free function; the production default ([`DefaultFindCut`]) composes the
//! ports of `_find_cut` and `_find_round_cut` exactly as Python does, and
//! the ported `test_ensure_budget_makes_progress_or_bails` injects
//! [`NoopFindCut`] (always an empty span) through the same seam.

use serde_json::{json, Value};

use crate::agent::session::{summary_message, Session, SUMMARY_MARKER};
use crate::backend::{estimate_prompt_tokens, Backend, BackendError};
use crate::config::Config;

/// Summarizer system prompt (verbatim from the reference).
pub const SUMMARIZER_SYSTEM: &str = "You compact an agent's conversation history to free \
context space. Write a concise, factual summary that lets the agent continue seamlessly. \
Preserve: decisions made, files created/edited (with exact paths), commands run and their \
outcomes, errors and how they were resolved, open tasks, and facts the user stated. Keep \
paths, names, and values exact. Do not invent new information. Output plain text only.";

/// `m.get("role") == "user" and str(m.get("content") or "").startswith(SUMMARY_MARKER)`.
pub fn is_summary(m: &Value) -> bool {
    m.get("role").and_then(Value::as_str) == Some("user")
        && m
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .starts_with(SUMMARY_MARKER)
}

/// Index just past the leading run of summary messages (after system).
pub fn front_summary_end(memory: &[Value]) -> usize {
    let mut i = 1usize; // memory[0] is the system message
    while i < memory.len() && is_summary(&memory[i]) {
        i += 1;
    }
    i
}

/// Indices of real user messages (turn starts), excluding summaries.
pub fn real_turn_starts(memory: &[Value]) -> Vec<usize> {
    memory
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.get("role").and_then(Value::as_str) == Some("user") && !is_summary(m)
        })
        .map(|(i, _)| i)
        .collect()
}

/// Index of the oldest real turn we keep, so the tail has exactly `keep`
/// complete turns. None when there is nothing old to drop.
pub fn find_cut(memory: &[Value], keep: u32) -> Option<usize> {
    let base = front_summary_end(memory);
    let turns = real_turn_starts(memory);
    if (turns.len() as u32) <= keep {
        return None;
    }
    let cut = turns[turns.len() - keep as usize];
    if cut <= base {
        // first kept turn is the first real turn: nothing before it
        return None;
    }
    Some(cut)
}

/// Cut index so the tail keeps the last `keep` rounds of the current turn,
/// walking backwards from the end. A round is an assistant message with
/// tool_calls plus its tool results; an in-flight round (results not all
/// back yet) counts as one and stays in the tail as-is. A trailing plain
/// assistant message (the turn's answer) is always kept. The cut lands on a
/// round's assistant message, so no assistant/tool group is ever split.
/// None when there is no round strictly before the kept window — dropping an
/// empty span would grow memory instead of shrinking it (summarizer call in,
/// no content out).
pub fn find_round_cut(memory: &[Value], base: usize, keep: u32) -> Option<usize> {
    let mut i = memory.len().saturating_sub(1);
    if i >= base
        && memory[i].get("role").and_then(Value::as_str) == Some("assistant")
        && memory[i].get("tool_calls").and_then(Value::as_array).is_none_or(Vec::is_empty)
    {
        i -= 1; // trailing answer: always kept
    }
    let mut starts: Vec<usize> = Vec::new(); // round assistant indices, newest first
    while i >= base {
        let role = memory[i].get("role").and_then(Value::as_str);
        if role == Some("tool") {
            // the walk lands here between rounds: back up to this round's
            // assistant message (always the one whose results these are)
            while i >= base && memory[i].get("role").and_then(Value::as_str) == Some("tool") {
                i -= 1;
            }
        }
        let m = &memory[i];
        if m.get("role").and_then(Value::as_str) != Some("assistant")
            || m.get("tool_calls").and_then(Value::as_array).is_none_or(Vec::is_empty)
        {
            break; // user msg, summary, or plain assistant
        }
        starts.push(i);
        i -= 1;
    }
    if (starts.len() as u32) <= keep {
        return None; // kept window covers every round
    }
    let cut = starts[keep as usize - 1]; // earliest kept round
    (cut > base).then_some(cut) // cut == base: nothing before it to drop
}

/// `str.replace("\n", " ")`, clipped to `n` chars with a `…`.
fn short(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        s
    } else {
        format!(
            "{}…",
            s.chars().take(n).collect::<String>()
        )
    }
}

/// Pick the most identifying argument of a tool call for the summary.
fn arg_detail(_name: &str, args_str: &str) -> String {
    let Ok(a) = (if args_str.is_empty() {
        serde_json::from_str::<Value>("{}")
    } else {
        serde_json::from_str(args_str)
    }) else {
        return short(args_str, 60);
    };
    let Some(obj) = a.as_object() else {
        return short(&a.to_string(), 60);
    };
    for key in ["path", "command", "url", "pattern", "query", "text"] {
        if let Some(v) = obj.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return short(v, 60);
        }
    }
    for v in obj.values() {
        if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
            return short(s, 60);
        }
    }
    short(&a.to_string(), 60)
}

/// Flatten dropped wire messages for the summarizer. Tool calls fold into
/// one line each:  called write_file(path) -> ok, 2,100 chars.
fn flatten(span: &[Value]) -> String {
    let mut results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in span.iter().filter(|m| m.get("role").and_then(Value::as_str) == Some("tool")) {
        results.insert(
            m.get("tool_call_id").and_then(Value::as_str).unwrap_or_default().to_string(),
            m.get("content").and_then(Value::as_str).unwrap_or_default().to_string(),
        );
    }
    let mut out: Vec<String> = Vec::new();
    for m in span {
        match m.get("role").and_then(Value::as_str) {
            Some("user") => {
                out.push(format!(
                    "user: {}",
                    m.get("content").and_then(Value::as_str).unwrap_or("")
                ));
            }
            Some("assistant") => {
                let content = m
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !content.is_empty() {
                    out.push(format!("assistant: {content}"));
                }
                if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
                    for tc in tcs {
                        let fn_ = tc.get("function").cloned().unwrap_or(Value::Null);
                        let name = fn_.get("name").and_then(Value::as_str).unwrap_or("?");
                        let id = tc.get("id").and_then(Value::as_str).unwrap_or_default();
                        let res = results.get(id).cloned().unwrap_or_default();
                        let status = if res.starts_with("error") { "err" } else { "ok" };
                        let args = fn_.get("arguments").and_then(Value::as_str).unwrap_or("");
                        out.push(format!(
                            "  called {}({}) -> {status}, {} chars",
                            name,
                            arg_detail(name, args),
                            res.len(),
                            // NOTE: Python's `:,` adds thousands separators;
                            // cosmetic difference in the summarizer prompt.
                        ));
                    }
                }
            }
            // tool results are folded into their assistant line above
            _ => {}
        }
    }
    out.join("\n")
}

/// A strategy for picking the compaction cut (see module docstring for the
/// Python-monkeypatch deviation). Production uses [`DefaultFindCut`].
pub trait FindCut {
    fn find(&self, memory: &[Value], base: usize, keep: u32) -> Option<usize>;
}

/// Production default: turn-level cut first, round-level cut as the
/// mid-turn fallback — exactly Python's `compact_once` body.
pub struct DefaultFindCut;

impl FindCut for DefaultFindCut {
    fn find(&self, memory: &[Value], _base: usize, keep: u32) -> Option<usize> {
        match find_cut(memory, keep) {
            Some(cut) => Some(cut),
            // No older user turns to drop (e.g. a single long tool-call
            // chain): fall back to cutting complete rounds, keeping the
            // last `keep`.
            None => find_round_cut(memory, front_summary_end(memory), keep),
        }
    }
}

/// Test injection: always an empty span (Python: `comp._find_cut = lambda
/// m, k: 2`).
pub struct NoopFindCut;

impl FindCut for NoopFindCut {
    fn find(&self, memory: &[Value], _base: usize, _keep: u32) -> Option<usize> {
        Some(front_summary_end(memory))
    }
}

/// One compaction pass: drop the oldest real turns (keeping the most recent
/// `keep`), summarize them, insert the summary. Returns the new memory, or
/// `Ok(None)` when there is nothing old to drop. `Err` if the summarizer
/// call fails or returns nothing (Python's `BackendError`).
pub async fn compact_once<F: FindCut>(
    memory: &[Value],
    client: &dyn Backend,
    cfg: &Config,
    session: &mut Session,
    find: &F,
    keep: Option<u32>,
) -> Result<Option<Vec<Value>>, BackendError> {
    let keep = keep.unwrap_or(cfg.context.keep_recent_turns);
    let base = front_summary_end(memory);
    let Some(cut) = find.find(memory, base, keep) else {
        return Ok(None);
    };
    if cut <= base {
        // Defensive (Python's `cut <= base` guards in the cut functions):
        // an empty span would grow memory — treat it as "nothing to drop".
        return Ok(None);
    }

    let flattened = flatten(&memory[base..cut]);
    let messages = [
        json!({ "role": "system", "content": SUMMARIZER_SYSTEM }),
        json!({
            "role": "user",
            "content": format!("Summarize this dropped conversation span:\n\n{flattened}"),
        }),
    ];
    let text = client
        .complete(&messages, cfg.context.summary_max_tokens)
        .await?;
    if text.trim().is_empty() {
        return Err(BackendError("summarizer returned an empty summary".into()));
    }
    let text = text.trim().to_string();

    session.record_summary(&text);
    record_kept_tail(session, &memory[cut..]);
    let mut new_memory: Vec<Value> = memory[..base].to_vec();
    new_memory.push(summary_message(&text));
    new_memory.extend_from_slice(&memory[cut..]);
    Ok(Some(new_memory))
}

/// Re-record the kept tail in the session log, AFTER the summary event
/// (tagged `dup: true` so it's obvious in the audit log).
///
/// `replay()` keeps "all summaries + all msg/tool events after the LAST
/// summary", so anything a cut leaves in memory must exist in the log after
/// the summary event — a resumed session otherwise loses it (e.g. the
/// current turn's rounds in a mid-turn cut, or kept prior turns that
/// predate the summary). The original events stay in the log unmodified as
/// the unsummarized audit trail. The summary and its tail are written
/// synchronously with no preemption point in between — that order is what
/// replay depends on.
fn record_kept_tail(session: &mut Session, tail: &[Value]) {
    for m in tail {
        match m.get("role").and_then(Value::as_str) {
            Some("user") => {
                session.append(json!({
                    "type": "msg", "role": "user",
                    "content": m.get("content").and_then(Value::as_str).unwrap_or(""),
                    "dup": true,
                }));
            }
            Some("assistant") => {
                let mut ev = json!({
                    "type": "msg", "role": "assistant",
                    "content": m.get("content").and_then(Value::as_str).unwrap_or(""),
                    "dup": true,
                });
                if let Some(tc) = m.get("tool_calls") {
                    ev["tool_calls"] = tc.clone();
                }
                // keep the thinking block so a resume replays it too
                if let Some(r) = m.get("reasoning_content").and_then(Value::as_str) {
                    if !r.is_empty() {
                        ev["reasoning_content"] = Value::String(r.to_string());
                    }
                }
                session.append(ev);
            }
            Some("tool") => {
                session.append(json!({
                    "type": "tool",
                    "id": m.get("tool_call_id").cloned().unwrap_or(Value::Null),
                    "result": m.get("content").and_then(Value::as_str).unwrap_or(""),
                    "ok": true,
                    "dup": true,
                }));
            }
            _ => {}
        }
    }
}

/// Auto path: compact until the estimate is under the trigger. Returns the
/// (possibly new) memory, or None when even the smallest recent window
/// exceeds the budget (an error event is recorded). An interrupt request
/// (cancel) stops the loop at the next iteration and returns memory as-is.
/// `Err` when a summarizer call fails (Python: `BackendError` propagates to
/// the loop, which records `compaction failed: …` and aborts the turn).
///
/// `cancel` is a plain `Fn() -> bool` so the caller can back it with
/// whatever interruption mechanism it uses (the agent loop's watch flag).
///
/// Safety: every pass must strictly shrink the memory (a summary replaces
/// >=2 messages with one). If that invariant ever fails we bail instead of
/// looping forever.
pub async fn ensure_budget<F: FindCut, C: Fn() -> bool>(
    memory: &[Value],
    client: &dyn Backend,
    cfg: &Config,
    session: &mut Session,
    cancel: Option<&C>,
    find: &F,
) -> Result<Option<Vec<Value>>, BackendError> {
    // Same trigger as the turn gate (Config::threshold_tokens) — one
    // source of truth so gate and loop can't drift apart.
    let target = cfg.context.threshold_tokens();
    let mut memory = memory.to_vec();
    let mut keep = cfg.context.keep_recent_turns;
    // The estimate is the only signal here: this loop re-checks CANDIDATE
    // memories (post-summary) that were never sent to the server, so no
    // live usage count exists for them.
    while estimate_prompt_tokens(&memory) > target {
        if cancel.is_some_and(|c| c()) {
            return Ok(Some(memory));
        }
        let new_mem = compact_once(&memory, client, cfg, session, find, Some(keep)).await?;
        match new_mem {
            None => {
                if keep > 1 {
                    // recent window too big: keep fewer, retry
                    keep = (keep / 2).max(1);
                    continue;
                }
                session.record_error("context exceeded, start a new session");
                return Ok(None);
            }
            Some(new_mem) => {
                if new_mem.len() >= memory.len() {
                    session.record_error("compaction made no progress, aborting");
                    return Ok(None);
                }
                memory = new_mem;
            }
        }
    }
    Ok(Some(memory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::SUMMARY_MARKER as MK;
    use crate::backend::StreamItem;
    use crate::config::{ContextConfig, WorkspaceConfig};
    use crate::backend::SseEvent;
    use futures_util::stream;
    use std::collections::HashSet;
    use std::sync::Mutex;

    struct MemCut;
    impl FindCut for MemCut {
        fn find(&self, memory: &[Value], _base: usize, keep: u32) -> Option<usize> {
            find_cut(memory, keep)
        }
    }

    fn user(s: &str) -> Value {
        json!({"role": "user", "content": s})
    }
    fn assistant(s: &str) -> Value {
        json!({"role": "assistant", "content": s})
    }
    fn summary(s: &str) -> Value {
        json!({"role": "user", "content": format!("{MK}\n{s}")})
    }
    fn round(i: u32, size: usize) -> [Value; 2] {
        [
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": format!("c{i}"), "type": "function",
                 "function": {"name": "run_command", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": format!("c{i}"),
                   "content": "z".repeat(size)}),
        ]
    }

    /// Memory of 3 turns; `find_cut(keep=2)` must land on turn 2's user msg.
    #[test]
    fn find_cut_keeps_last_n_turns() {
        let mem = vec![
            json!({"role": "system", "content": "s"}),
            user("t0"),
            assistant("a0"),
            user("t1"),
            assistant("a1"),
            user("t2"),
            assistant("a2"),
        ];
        assert_eq!(find_cut(&mem, 2), Some(3));
        assert_eq!(find_cut(&mem, 1), Some(5));
        // nothing old to drop
        assert_eq!(find_cut(&mem, 3), None);
        assert_eq!(find_cut(&mem, 4), None);
        // single turn: the kept window covers everything
        assert_eq!(find_cut(&mem[..3], 1), None);
    }

    #[test]
    fn find_cut_ignores_leading_summaries() {
        let mem = vec![
            json!({"role": "system", "content": "s"}),
            summary("old"),
            user("t0"),
            assistant("a0"),
            user("t1"),
            assistant("a1"),
        ];
        assert_eq!(front_summary_end(&mem), 2);
        // summaries are not turn starts: keep=1 keeps t1, drops t0
        assert_eq!(find_cut(&mem, 1), Some(4));
        // keep=2 covers both turns -> nothing to drop
        assert_eq!(find_cut(&mem, 2), None);
        let mem2 = vec![
            json!({"role": "system", "content": "s"}),
            summary("old"),
            user("t0"),
            assistant("a0"),
            user("t1"),
            assistant("a1"),
            user("t2"),
            assistant("a2"),
        ];
        assert_eq!(find_cut(&mem2, 1), Some(6));
    }

    #[test]
    fn round_cut_walks_backwards_and_keeps_last_n() {
        let mut mem = vec![
            json!({"role": "system", "content": "s"}),
            user("task"),
        ];
        for i in 0..10u32 {
            mem.extend_from_slice(&round(i, 100));
        }
        // no trailing answer: keep the last 2 rounds -> cut on c8
        let cut = find_round_cut(&mem, 1, 2).expect("cut");
        assert_eq!(mem[cut]["role"], "assistant");
        assert_eq!(mem[cut]["tool_calls"][0]["id"], "c8");

        // trailing plain answer is kept but is NOT a round: the kept rounds
        // are still c9+c8, so the cut stays on c8
        mem.push(assistant("final answer"));
        let cut2 = find_round_cut(&mem, 1, 2).expect("cut");
        assert_eq!(mem[cut2]["tool_calls"][0]["id"], "c8");

        // in-flight round (results not all back) counts as ONE round and,
        // with keep=1, is the earliest kept round
        let mut mem3 = vec![
            json!({"role": "system", "content": "s"}),
            user("task"),
        ];
        for i in 0..10u32 {
            mem3.extend_from_slice(&round(i, 100));
        }
        mem3.push(json!({"role": "assistant", "content": "", "tool_calls": [
            {"id": "g0", "type": "function", "function": {"name": "x", "arguments": "{}"}},
            {"id": "g1", "type": "function", "function": {"name": "x", "arguments": "{}"}}]}));
        mem3.push(json!({"role": "tool", "tool_call_id": "g0", "content": "r"}));
        let cut3 = find_round_cut(&mem3, 1, 1).expect("cut");
        assert_eq!(mem3[cut3]["tool_calls"][0]["id"], "g0");

        // a kept window that covers every round lands on the user message
        // (== base-1): dropping an empty span is rejected -> None
        assert_eq!(find_round_cut(&mem3, 1, 11), None);
    }

    #[test]
    fn flatten_folds_tool_calls_with_status() {
        let span = vec![
            user("do it"),
            json!({"role": "assistant", "content": "working", "tool_calls": [
                {"id": "c1", "type": "function",
                 "function": {"name": "write_file",
                              "arguments": "{\"path\": \"a.txt\", \"content\": \"hi\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "wrote 2 chars"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c2", "type": "function",
                 "function": {"name": "run_command",
                              "arguments": "{\"command\": \"ls\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "error: no such file"}),
        ];
        let flat = flatten(&span);
        assert!(flat.starts_with("user: do it\nassistant: working"), "{flat}");
        assert!(flat.contains("called write_file(a.txt) -> ok, 13 chars"), "{flat}");
        assert!(flat.contains("called run_command(ls) -> err, 19 chars"), "{flat}");
    }

    #[test]
    fn arg_detail_prefers_identifying_keys() {
        assert_eq!(arg_detail("run_command", r#"{"command": "make test", "x": "y"}"#), "make test");
        assert_eq!(arg_detail("web_search", r#"{"query": "rust async"}"#), "rust async");
        // no known key: first non-empty string value
        assert_eq!(arg_detail("mystery", r#"{"foo": "bar"}"#), "bar");
        // non-dict JSON falls back to the raw text
        assert_eq!(arg_detail("x", "42"), "42");
        // unparseable: short raw
        assert_eq!(arg_detail("x", "{not json"), "{not json");
        // long values get clipped
        assert_eq!(
            arg_detail("run_command", &format!("{{\"command\": \"{}\"}}", "a".repeat(100))),
            format!("{}…", "a".repeat(60))
        );
    }

    // ---- end-to-end through a fake backend -------------------------------

    struct Fake;
    static N: Mutex<u32> = Mutex::new(0);

    impl Backend for Fake {
        fn stream_chat<'a>(
            &'a self,
            _m: &'a [Value],
            _t: Option<&'a [Value]>,
        ) -> crate::backend::ChatStream<'a> {
            Box::pin(async {
                Ok(Box::pin(stream::iter([Ok(StreamItem::Event(
                    SseEvent::Text("x".into()),
                )), Ok(StreamItem::Done {
                    finish_reason: "stop".into(),
                    usage: None,
                })])) as _)
            })
        }
        fn complete<'a>(&'a self, _m: &'a [Value], _t: u64) -> crate::backend::CompleteFut<'a> {
            let n = *N.lock().unwrap_or_else(|e| e.into_inner()) + 1;
            *N.lock().unwrap_or_else(|e| e.into_inner()) = n;
            Box::pin(async move { Ok(format!("summary #{n}")) })
        }
    }

    fn cfg() -> Config {
        Config {
            context: ContextConfig {
                max_tokens: 8000,
                summarize_threshold: 0.5,
                keep_recent_turns: 2,
                ..ContextConfig::default()
            },
            workspace: WorkspaceConfig {
                path: std::env::temp_dir()
                    .join(format!("harness-comp-ws-{}", std::process::id()))
                    .to_string_lossy()
                    .into_owned(),
                auto_git: false,
            },
            ..Config::default()
        }
    }

    fn make_session() -> Session {
        let dir = std::env::temp_dir().join(format!(
            "harness-comp-sess-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        Session::create("coder", "m", Some(&dir))
    }

    /// Every tool message must match a tool_call id of an earlier assistant
    /// message (port of the reference test helper).
    fn assert_groups_intact(memory: &[Value]) {
        let mut ids: HashSet<&str> = HashSet::new();
        for m in memory {
            match m.get("role").and_then(Value::as_str) {
                Some("assistant") => {
                    if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
                        for tc in tcs {
                            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                                ids.insert(id);
                            }
                        }
                    }
                }
                Some("tool") => {
                    let id = m["tool_call_id"].as_str().unwrap_or("");
                    assert!(ids.contains(id), "dangling tool result: {memory:?}");
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn compact_once_turn_cut_and_round_cut() {
        *N.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        let client = Fake;
        let cfg = cfg();

        // --- turn-level cut across multi-turn memory ---
        let mut session = make_session();
        let mut mem = vec![json!({"role": "system", "content": "s"})];
        for i in 0..10u32 {
            session.record_user(&format!("task {i}"));
            mem.push(user(&format!("task {i} {}", "x".repeat(800))));
            session.record_assistant(&json!(format!("done {i} {}", "y".repeat(800))), None, None);
            mem.push(assistant(&format!("done {i} {}", "y".repeat(800))));
        }
        let new_mem = compact_once(&mem, &client, &cfg, &mut session, &MemCut, None)
            .await
            .unwrap()
            .expect("must compact");
        assert!(new_mem.len() < mem.len());
        // memory[1] is the new summary; the tail kept exactly 2 turns
        assert!(is_summary(&new_mem[1]));
        assert!(new_mem[1]["content"].as_str().unwrap().contains("summary #1"));
        assert_groups_intact(&new_mem);

        // --- round-level cut inside a single long tool chain ---
        *N.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        let mut session = make_session();
        session.record_user("one big task");
        let mut mem = vec![
            json!({"role": "system", "content": "s"}),
            user("one big task"),
        ];
        for i in 0..30u32 {
            mem.extend_from_slice(&round(i, 1000));
        }
        let new_mem = compact_once(&mem, &client, &cfg, &mut session, &DefaultFindCut, None)
            .await
            .unwrap()
            .expect("round cut must fire");
        assert!(new_mem.len() < mem.len(), "compaction must shrink memory");
        assert_groups_intact(&new_mem);
    }
}
