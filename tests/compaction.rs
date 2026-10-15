//! Compaction + replay-consistency tests.
//!
//! Port of `reference/harness/tests/test_compaction.py` — the acceptance
//! gate for the compaction invariants (replay consistency, group
//! integrity, strict shrink, mid-turn round cuts; plan §8).
//!
//! `FakeClient` (the reference) becomes `impl Backend`; the
//! monkeypatch-able `_find_cut` global becomes the `FindCut` injection
//! seam (`NoopFindCut`) — documented deviation, see
//! `agent/compaction.rs` module docstring.
//!
//! `Session::create` reads `$HARNESS_STATE`, so every test holds the
//! process-global env lock and points it at its own temp dir (pattern from
//! `tests/seed.rs`).

use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::stream;
use harness::agent::compaction::{
    compact_once, ensure_budget, find_round_cut, DefaultFindCut, NoopFindCut,
};
use harness::agent::loop_::AgentLoop;
use harness::agent::session::Session;
use harness::backend::{Backend, BackendError, CompleteFut, ChatStream, SseEvent, StreamItem, Usage};
use harness::config::Config;
use serde_json::{json, Value};

const SYS: &str = "sys prompt";

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn tmp(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "harness-comp-test-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// Set both harness env vars to throwaway dirs (under the env lock).
fn set_env(state: &std::path::Path, ws: &std::path::Path) {
    std::fs::create_dir_all(state).unwrap();
    std::fs::create_dir_all(ws).unwrap();
    // SAFETY: held `env_lock` guarantees no other test touches env vars
    // concurrently in this test process.
    unsafe {
        std::env::set_var("HARNESS_STATE", state);
        std::env::set_var("HARNESS_WORKSPACE", ws);
    }
}

fn make_cfg(ws: &std::path::Path, ctx: serde_json::Value) -> Config {
    let raw = json!({
        "context": ctx,
        "workspace": {"path": ws.to_string_lossy(), "auto_git": false},
    });
    harness::config::build_config(raw)
}

/// Reference `FakeClient`: every `complete()` call returns `summary #N`
/// with a global counter.
struct FakeClient {
    n: Mutex<u32>,
}

impl FakeClient {
    fn new() -> Self {
        Self {
            n: Mutex::new(0),
        }
    }
}

impl Backend for FakeClient {
    fn stream_chat<'a>(
        &'a self,
        _m: &'a [Value],
        _t: Option<&'a [Value]>,
    ) -> ChatStream<'a> {
        Box::pin(async { unreachable!("FakeClient has no stream") })
    }
    fn complete<'a>(&'a self, _m: &'a [Value], _t: u64) -> CompleteFut<'a> {
        let n = *self.n.lock().unwrap_or_else(|e| e.into_inner()) + 1;
        *self.n.lock().unwrap_or_else(|e| e.into_inner()) = n;
        Box::pin(async move { Ok(format!("summary #{n}")) })
    }
}

/// live memory (sans system) must equal replay() (sans system).
fn assert_replay_consistent(session: &Session, live_mem: &[Value]) {
    let replayed = session.replay(SYS);
    let live: Vec<&Value> = live_mem.iter().filter(|m| m["role"] != "system").collect();
    let rp: Vec<&Value> = replayed.iter().filter(|m| m["role"] != "system").collect();
    assert_eq!(
        live, rp,
        "replay diverged: live={} msgs, replay={}",
        live.len(),
        rp.len()
    );
}

/// Every tool message must match a tool_call id of an earlier assistant.
fn assert_groups_intact(memory: &[Value]) {
    let mut ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
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
                assert!(ids.contains(id), "dangling tool result after compaction");
            }
            _ => {}
        }
    }
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

fn default_ctx() -> serde_json::Value {
    json!({"max_tokens": 8000, "summarize": "auto",
           "summarize_threshold": 0.5, "keep_recent_turns": 2})
}

#[tokio::test]
async fn test_multi_turn_compaction_and_resume() {
    let _lock = env_lock();
    let (state, ws) = (tmp("state-mt"), tmp("ws-mt"));
    set_env(&state, &ws);
    let cfg = make_cfg(&ws, default_ctx());
    let mut session = Session::create("coder", "m", None);
    let client = FakeClient::new();

    let mut mem = vec![json!({"role": "system", "content": SYS})];
    for i in 0..10u32 {
        session.record_user(&format!("task {i}"));
        mem.push(json!({"role": "user", "content": format!("task {i} {}", "x".repeat(800))}));
        session.record_assistant(&json!(format!("done {i} {}", "y".repeat(800))), None, None);
        mem.push(json!({"role": "assistant", "content": format!("done {i} {}", "y".repeat(800))}));
    }
    let mem = ensure_budget(&mem, &client, &cfg, &mut session, Some(&|| false), &DefaultFindCut)
        .await
        .unwrap()
        .unwrap_or(mem);
    assert_replay_consistent(&session, &mem);
    assert_replay_consistent(&Session::open(&session.path), &mem);
    assert_groups_intact(&mem);

    // second compaction: two summaries, still consistent
    let mut mem = mem;
    for i in 10..20u32 {
        session.record_user(&format!("task {i}"));
        mem.push(json!({"role": "user", "content": format!("task {i} {}", "x".repeat(800))}));
        session.record_assistant(&json!(format!("done {i} {}", "y".repeat(800))), None, None);
        mem.push(json!({"role": "assistant", "content": format!("done {i} {}", "y".repeat(800))}));
    }
    let mem = ensure_budget(&mem, &client, &cfg, &mut session, Some(&|| false), &DefaultFindCut).await
        .unwrap()
        .unwrap_or(mem);
    assert_replay_consistent(&session, &mem);
    assert_replay_consistent(&Session::open(&session.path), &mem);
    // two summary events recorded
    let summaries: Vec<_> = session
        .events()
        .iter()
        .filter(|e| e["type"] == "summary")
        .collect();
    assert_eq!(summaries.len(), 2, "{:?}", session.events());
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn test_single_long_tool_chain_compacts_mid_turn() {
    // One user turn + a long tool chain: nothing "old", but compaction must
    // still fire via a round-level cut (regression: used to return None and,
    // with a leading summary present, loop forever on an empty span).
    let _lock = env_lock();
    let (state, ws) = (tmp("state-lc"), tmp("ws-lc"));
    set_env(&state, &ws);
    let cfg = make_cfg(&ws, default_ctx());
    let mut session = Session::create("coder", "m", None);
    let client = FakeClient::new();

    session.record_user("one big task");
    let mut mem = vec![
        json!({"role": "system", "content": SYS}),
        json!({"role": "user", "content": "one big task"}),
    ];
    for i in 0..30u32 {
        mem.extend_from_slice(&round(i, 1000));
    }
    let cut = find_round_cut(&mem, 1, 2);
    assert!(cut.is_some_and(|c| c > 1), "round cut must drop real rounds");

    let before = mem.len();
    let mut mem = compact_once(&mem, &client, &cfg, &mut session, &DefaultFindCut, None)
        .await
        .unwrap()
        .expect("compaction must shrink memory");
    assert!(mem.len() < before, "compaction must shrink memory");
    assert_replay_consistent(&session, &mem);
    assert_replay_consistent(&Session::open(&session.path), &mem);
    assert_groups_intact(&mem);

    // a further round cut: summaries accumulate, consistency holds
    for i in 30..60u32 {
        mem.extend_from_slice(&round(i, 1000));
    }
    let mem = compact_once(&mem, &client, &cfg, &mut session, &DefaultFindCut, None)
        .await
        .unwrap()
        .expect("second compaction");
    assert_replay_consistent(&session, &mem);
    assert_groups_intact(&mem);
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn test_round_cut_keeps_trailing_answer_and_inflight_round() {
    let _lock = env_lock();
    let (state, ws) = (tmp("state-if"), tmp("ws-if"));
    set_env(&state, &ws);
    let cfg = make_cfg(&ws, default_ctx());
    let mut session = Session::create("coder", "m", None);
    let client = FakeClient::new();

    session.record_user("task");
    let mut mem = vec![
        json!({"role": "system", "content": SYS}),
        json!({"role": "user", "content": "task"}),
    ];
    for i in 0..10u32 {
        mem.extend_from_slice(&round(i, 1500));
    }
    // in-flight round: 2 calls, only 1 result
    mem.push(json!({"role": "assistant", "content": "", "tool_calls": [
        {"id": "g0", "type": "function", "function": {"name": "x", "arguments": "{}"}},
        {"id": "g1", "type": "function", "function": {"name": "x", "arguments": "{}"}}]}));
    mem.push(json!({"role": "tool", "tool_call_id": "g0", "content": "r"}));

    let mem2 = compact_once(&mem, &client, &cfg, &mut session, &DefaultFindCut, None)
        .await
        .unwrap()
        .expect("in-flight round must compact");
    // 2 + 20 + 2 = 24 msgs in; the cut is on c9 (keep=2 keeps c9 + the
    // in-flight round), and the dropped span runs from base=1 — so the
    // turn's own user message is folded in too (24 - 19 + 1 = 6)
    assert_eq!(mem2.len(), 6, "{mem2:?}");
    assert_eq!(mem2.last().unwrap()["role"], "tool");
    assert_eq!(mem2.last().unwrap()["tool_call_id"], "g0");
    assert_eq!(mem2[mem2.len() - 2]["role"], "assistant");
    assert_eq!(mem2[mem2.len() - 2]["tool_calls"].as_array().unwrap().len(), 2);
    assert_replay_consistent(&session, &mem2);
    assert_groups_intact(&mem2);
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn test_ensure_budget_makes_progress_or_bails() {
    // If a pass ever fails to shrink memory, ensure_budget must bail
    // (regression: empty-span "compaction" looped forever when a leading
    // summary existed and the cut collapsed onto base).
    //
    // Python monkeypatches `comp._find_cut`; the Rust equivalent is the
    // `FindCut` injection seam (documented deviation, see
    // `agent/compaction.rs`): `NoopFindCut` returns cut == base.
    let _lock = env_lock();
    let (state, ws) = (tmp("state-ep"), tmp("ws-ep"));
    set_env(&state, &ws);
    let cfg = make_cfg(&ws, default_ctx());
    let mut session = Session::create("coder", "m", None);
    let client = FakeClient::new();

    let mem = vec![
        json!({"role": "system", "content": SYS}),
        json!({"role": "user", "content": "[Summary of prior conversation]\nold"}),
        json!({"role": "user", "content": format!("new {}", "x".repeat(20000))}),
        json!({"role": "assistant", "content": "a".repeat(20000)}),
    ];
    let out = ensure_budget(&mem, &client, &cfg, &mut session, Some(&|| false), &NoopFindCut)
        .await
        .expect("no backend error expected");
    assert!(out.is_none(), "ensure_budget must abort when compaction makes no progress");
    // error event recorded
    assert!(session
        .events()
        .iter()
        .any(|e| e["type"] == "error"),
        "{:?}",
        session.events()
    );
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&ws);
}

/// Reference `ChainClient`: rounds 1..=6 stream a `run_command` tool call
/// (real shell: ~20k of z's), rounds > 6 stream the final text.
struct ChainClient {
    round: std::sync::atomic::AtomicU32,
    compact_calls: std::sync::atomic::AtomicU32,
    prompt_sizes: Mutex<Vec<u64>>,
}

impl ChainClient {
    fn new() -> Self {
        Self {
            round: std::sync::atomic::AtomicU32::new(0),
            compact_calls: std::sync::atomic::AtomicU32::new(0),
            prompt_sizes: Mutex::new(Vec::new()),
        }
    }
}

impl Backend for ChainClient {
    fn stream_chat<'a>(
        &'a self,
        messages: &'a [Value],
        _t: Option<&'a [Value]>,
    ) -> ChatStream<'a> {
        let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        // What the REAL server would report: the token count of what we are
        // actually sending this request (the fake approximates it the same
        // way as estimate_prompt_tokens: chars/4).
        let est_prompt = messages.iter().map(|m| m["content"].as_str().unwrap_or("").len() as u64).sum::<u64>() / 4;
        self.prompt_sizes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(est_prompt);
        let items: Vec<Result<StreamItem, BackendError>> = if round <= 6 {
            let args = json!({"command": "head -c 20000 /dev/zero | tr '\\0' z"});
            vec![
                Ok(StreamItem::Event(SseEvent::ToolCall {
                    index: 0,
                    id: Some(format!("call_{round}")),
                    name: Some("run_command".into()),
                    arguments: args.to_string(),
                })),
                Ok(StreamItem::Done {
                    finish_reason: "tool_calls".into(),
                    usage: Some(Usage { prompt_tokens: est_prompt, completion_tokens: 10 }),
                }),
            ]
        } else {
            vec![
                Ok(StreamItem::Event(SseEvent::Text("done".into()))),
                Ok(StreamItem::Done {
                    finish_reason: "stop".into(),
                    usage: Some(Usage { prompt_tokens: est_prompt, completion_tokens: 5 }),
                }),
            ]
        };
        Box::pin(async { Ok(Box::pin(stream::iter(items)) as _) })
    }
    fn complete<'a>(&'a self, _m: &'a [Value], _t: u64) -> CompleteFut<'a> {
        self.compact_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok("compact summary of earlier rounds".to_string()) })
    }
}

#[tokio::test]
async fn test_loop_checks_budget_mid_chain() {
    // End-to-end through run_turn: a single turn whose tool rounds each add
    // ~20k chars must trigger compaction INSIDE the tool chain, not only at
    // the next turn start.
    let _lock = env_lock();
    let (state, ws) = (tmp("state-loop"), tmp("ws-loop"));
    set_env(&state, &ws);
    let cfg = make_cfg(
        &ws,
        json!({"max_tokens": 12000, "summarize": "auto",
               "summarize_threshold": 0.7, "keep_recent_turns": 2}),
    );
    // tools.limits override (exec_output_chars big enough for 20k results)
    let cfg = Config {
        tools: harness::config::ToolsConfig {
            limits: harness::config::LimitsConfig {
                exec_output_chars: 100_000,
                max_tool_rounds: 50,
                exec_timeout_sec: 10,
                ..Default::default()
            },
            ..Default::default()
        },
        ..cfg
    };

    harness::tools::register_all();
    let session = Session::create("coder", "m", None);
    let session_path = session.path.clone();
    let client = Arc::new(ChainClient::new());
    let mut loop_ = AgentLoop::new(
        cfg,
        client.clone(),
        session,
        Some(Arc::new(|_n, _a, _p| "y".to_string())),
        Some(Arc::new(|_k, _d| {})),
    );
    let res = loop_.run_turn("do a long multi-step task").await;
    assert!(res.ok, "turn failed: {}", res.reason);
    assert!(client.compact_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1, "compaction must fire inside the tool chain");
    // unsummarized the 6 rounds would be ~33k est tokens; it must stay small
    let sizes = client.prompt_sizes.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        sizes.iter().copied().max().unwrap_or(0) < 30000,
        "prompt grew too large: {sizes:?}"
    );
    // session log still replays to the live memory
    assert_replay_consistent(&Session::open(&session_path), &loop_.memory);
    let _ = std::fs::remove_dir_all(&state);
    let _ = std::fs::remove_dir_all(&ws);
}
