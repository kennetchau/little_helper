//! The agent turn state machine (async port of
//! `reference/harness/src/harness/agent/loop.py`).
//!
//! One `run_turn(user_message)` call drives a full turn:
//!
//! ```text
//! user message -> budget check (compaction) -> streaming request ->
//! [tool calls -> approval -> execute -> tool results]* -> final message ->
//! wrap-up (final diff + the two independent auto-commits)
//! ```
//!
//! Memory is a plain `Vec<Value>` in OpenAI wire format, kept in lockstep
//! with the session's JSONL log: every append to memory is paired with a
//! session record, so a resumed session replays to exactly the same memory.
//!
//! Tool errors are data for the model; only backend/loop failures abort the
//! turn. An aborted or interrupted turn still commits its side effects.
//!
//! Cancel is a `tokio::sync::watch` bool (plan §2): checked at the same
//! safe points as Python's `threading.Event`, plus `tokio::select!`
//! around the stream so an in-flight stream stops promptly. A watch bool
//! mirrors `threading.Event` one-for-one — `cancel()` is `event.set()`,
//! the turn-start reset is `event.clear()`, and the safe-point checks are
//! `event.is_set()`. Approval and output are injectable sync callbacks
//! (`Arc<dyn Fn>`), exactly like Python's `ask_approval`/`emit`
//! callables — the TUI (phase 6) feeds them from its own event loop.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::{json, Value};
use similar::TextDiff;

use crate::agent::compaction::{ensure_budget, DefaultFindCut};
use crate::agent::prompts;
use crate::agent::session::Session;
use crate::backend::{estimate_prompt_tokens, Backend, BackendError, SseEvent, StreamItem, Usage};
use crate::config::{self, Config};
use crate::state::gitstore;
use crate::tools::registry::{self, ToolContext, ToolResult};

pub const DEFAULT_AGENT: &str = "coder";

/// Outcome of one `run_turn`. The loop never raises for turn-level
/// failures (port of Python's frozen `TurnResult` dataclass).
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub ok: bool,
    pub reason: String,
    pub commit: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl TurnResult {
    fn abort(reason: &str, prompt_tokens: u64, completion_tokens: u64) -> Self {
        Self {
            ok: false,
            reason: reason.to_string(),
            commit: None,
            prompt_tokens,
            completion_tokens,
        }
    }
}

type AskFn = Arc<dyn Fn(&str, &Value, &str) -> String + Send + Sync>;
type EmitFn = Arc<dyn Fn(&str, &Value) + Send + Sync>;

// ---------- small helpers (port of the module-level functions) ----------

fn git_out(args: &[&str], cwd: &std::path::Path) -> String {
    let Ok(out) = std::process::Command::new("git").args(args).current_dir(cwd).output()
    else {
        return String::new();
    };
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn short_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let cut: String = text.chars().take(limit).collect();
    let nl = cut.rfind('\n').filter(|&n| n > cut.len() / 2);
    let cut = match nl {
        Some(n) => cut[..n].to_string(),
        None => cut,
    };
    format!("{cut}\n…[truncated at {limit} chars]")
}

/// First non-empty line of the final reply, for the commit message.
fn first_line(text: &str, limit: usize) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let Some(line) = lines.first().copied() else {
        return "(no summary)".to_string();
    };
    if line.chars().count() <= limit {
        line.to_string()
    } else {
        format!("{}…", line.chars().take(limit - 1).collect::<String>())
    }
}

fn arg_str(args: &Value, first: &str, second: &str) -> String {
    args.get(first)
        .or_else(|| args.get(second))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Unified diff for `edit_file` when the file exists; old/new strings
/// otherwise (port of `_edit_preview`).
fn edit_preview(args: &Value, workspace: &std::path::Path) -> String {
    let path = arg_str(args, "path", "file");
    let old = arg_str(args, "old_string", "old");
    let new = arg_str(args, "new_string", "new");
    if !path.is_empty() {
        let candidate = workspace.join(&path);
        if candidate.is_file() {
            if let Ok(original) = std::fs::read_to_string(&candidate) {
                if !old.is_empty() {
                    let replaced = if args.get("replace_all").and_then(Value::as_bool) == Some(true)
                    {
                        original.replace(&old, &new)
                    } else {
                        original.replacen(&old, &new, 1)
                    };
                    let diff_obj = TextDiff::from_lines(&original, &replaced);
                    let diff = diff_obj
                        .unified_diff()
                        .to_string()
                        .lines()
                        .map(|line| {
                            if line.starts_with("---") {
                                format!("--- a/{path}")
                            } else if line.starts_with("+++") {
                                format!("+++ b/{path}")
                            } else {
                                line.to_string()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !diff.trim().is_empty() {
                        return short_text(&format!("{path}\n{diff}"), 4000);
                    }
                }
            }
        }
    }
    short_text(&format!("{path}\n-find:\n{old}\n+replace:\n{new}"), 2000)
}

/// What the user approves: diff for edits, command text for exec, URL for
/// web (port of `make_preview`).
pub fn make_preview(name: &str, args: &Value, workspace: &std::path::Path) -> String {
    let args = if args.is_null() { json!({}) } else { args.clone() };
    match name {
        "write_file" => short_text(
            &format!(
                "{}\n{}",
                args.get("path").and_then(Value::as_str).unwrap_or(""),
                args.get("content").and_then(Value::as_str).unwrap_or("")
            ),
            2000,
        ),
        "edit_file" => edit_preview(&args, workspace),
        "run_command" => format!(
            "$ {}",
            args.get("command").and_then(Value::as_str).unwrap_or("")
        ),
        "web_fetch" => format!(
            "GET {}",
            args.get("url").and_then(Value::as_str).unwrap_or("")
        ),
        "web_search" => format!(
            "search: {}",
            args.get("query").and_then(Value::as_str).unwrap_or("")
        ),
        "delete_file" => format!(
            "delete {}",
            args.get("path").and_then(Value::as_str).unwrap_or("")
        ),
        _ => short_text(&args.to_string(), 1500),
    }
}

fn default_ask(name: &str, _args: &Value, preview: &str) -> String {
    // Console approval prompt. EOF denies, so unattended runs never hang
    // (Python: EOFError/KeyboardInterrupt -> "n").
    eprintln!("===== {name} — approval requested =====");
    eprintln!("{preview}");
    let mut answer = String::new();
    eprint!("[y]es / [n]o / [a]lways-allow this tool: ");
    use std::io::Write;
    let _ = std::io::stderr().flush();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return "n".to_string();
    }
    match answer.trim().to_lowercase().as_str() {
        "y" | "yes" => "y".to_string(),
        "a" | "always" => "a".to_string(),
        _ => "n".to_string(),
    }
}

fn default_emit(kind: &str, data: &Value) {
    // Headless renderer. The TUI (phase 6) passes its own emit() instead.
    match kind {
        "text" | "reasoning" => {
            print!("{}", data.get("text").and_then(Value::as_str).unwrap_or(""));
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
        "tool" => {
            let args = data.get("args").cloned().unwrap_or(Value::Null);
            let shown = if args.is_object() { args } else { json!({"raw": args}) };
            eprintln!(
                "\n⚙ {} {}",
                data.get("name").and_then(Value::as_str).unwrap_or("?"),
                short_text(&shown.to_string(), 200)
            );
        }
        "tool_result" => {
            let mark = if data.get("ok").and_then(Value::as_bool) == Some(true) { "✓" } else { "✗" };
            eprintln!(
                "  {mark} {}",
                short_text(data.get("result").and_then(Value::as_str).unwrap_or(""), 400)
            );
        }
        "error" => eprintln!(
            "\nerror: {}",
            data.get("text").and_then(Value::as_str).unwrap_or("")
        ),
        _ => eprintln!(
            "\n{}",
            data.get("text").and_then(Value::as_str).unwrap_or("")
        ),
    }
}

/// Build the assistant wire message. `reasoning_content` is included only
/// when non-empty (the llama.cpp server parses it from assistant messages
/// and feeds it back into the model's thinking channel); a thinking model
/// may also emit reasoning on a tool-calling round, so both shapes carry it.
fn assistant_msg(content: &str, reasoning: &str, tool_calls: Option<&Value>) -> Value {
    let mut msg = json!({ "role": "assistant", "content": content });
    if !reasoning.is_empty() {
        msg["reasoning_content"] = Value::String(reasoning.to_string());
    }
    if let Some(tc) = tool_calls {
        msg["tool_calls"] = tc.clone();
    }
    msg
}

// ---------- the loop ----------

/// One streamed response: text, thinking, tool calls, usage.
///
/// `reasoning` carries the `reasoning_content` the backend streamed. Unlike
/// the Python reference (which used it for display only), the Rust harness
/// echoes it back on the assistant message — the llama.cpp server accepts
/// `reasoning_content` on assistant messages and renders it back into the
/// model's own thinking channel (see llama.cpp `common/chat.cpp`
/// from_oaicompat and `tools/server/server-task.cpp`).
pub struct StreamResult {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<Value>,
    pub usage: Option<Usage>,
}

pub struct AgentLoop {
    pub cfg: Config,
    pub client: Arc<dyn Backend>,
    pub session: Session,
    pub ctx: ToolContext,
    pub memory: Vec<Value>,
    schemas: Vec<Value>,
    enabled: HashSet<String>,
    ask: AskFn,
    emit: EmitFn,
    /// Session-scoped "always allow" approvals.
    always_allow: HashSet<String>,
    /// Interrupt request: Python's `threading.Event`. `true` == set.
    cancel_tx: tokio::sync::watch::Sender<bool>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    /// `memory` length at the last request: `memory[..len]` was tokenized
    /// by the server (covered by the live prompt count); only the tail
    /// appended since has a local chars/4 estimate.
    requested_mem_len: usize,
    /// The backend reported server-side prompt tokens on the last request.
    /// When false (fresh session / backend without usage) the budget gate
    /// falls back to the chars/4 estimate of the whole memory.
    live_usage: bool,
}

impl AgentLoop {
    /// Request interruption of the running turn (thread-safe; Python's
    /// `AgentLoop.cancel` / `Event.set()`). The loop stops at the next safe
    /// point (between stream chunks, between tool calls, between rounds),
    /// discards the partial output, records an interrupted event, and wraps
    /// up like any other turn. A tool that is already executing runs on to
    /// completion or its own timeout.
    pub fn cancel(&self) {
        let _ = self.cancel_tx.send(true);
    }

    /// Python's `Event.clear()` — start-of-turn reset.
    fn cancel_clear(&self) {
        let _ = self.cancel_tx.send(false);
    }

    /// Python's `Event.is_set()` — read the current flag.
    fn cancel_flag(&self) -> bool {
        *self.cancel_rx.borrow()
    }

    /// A second, lock-free handle to the cancel flag — so a caller can
    /// cancel a running turn while the turn task holds `&mut self`
    /// (Python's `Event.set()` is usable from any thread for the same
    /// reason; the TUI uses it for Ctrl-C).
    pub(crate) fn cancel_sender(&self) -> tokio::sync::watch::Sender<bool> {
        self.cancel_tx.clone()
    }

    /// Future that resolves when the flag is set (or already was) — used
    /// inside `tokio::select!` to stop an in-flight stream promptly.
    async fn wait_cancel(&self) {
        let mut rx = self.cancel_rx.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

impl AgentLoop {
    pub fn new(
        cfg: Config,
        client: Arc<dyn Backend>,
        session: Session,
        ask_approval: Option<AskFn>,
        emit: Option<EmitFn>,
    ) -> Self {
        let ctx = ToolContext {
            cfg: cfg.clone(),
            workspace: PathBuf::from(cfg.workspace.path.clone()),
        };
        let schemas = registry::schemas(&cfg);
        let enabled: HashSet<String> =
            registry::enabled_names(&cfg).into_iter().map(str::to_string).collect();
        // Composed default system prompt (base + agent-folder notes, plan
        // §4.1); `prompt` session events override it in replay.
        let system = prompts::system_prompt(&session.agent(), &ctx.workspace);
        let memory = session.replay(&system);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        Self {
            cfg,
            client,
            session,
            ctx,
            memory,
            schemas,
            enabled,
            ask: ask_approval.unwrap_or_else(|| Arc::new(default_ask)),
            emit: emit.unwrap_or_else(|| Arc::new(default_emit)),
            always_allow: HashSet::new(),
            cancel_tx,
            cancel_rx,
            requested_mem_len: 0,
            live_usage: false,
        }
    }

    // -- system prompt (phase 5, plan §4.2) ---------------------------------

    /// Replace the system prompt mid-session.
    ///
    /// Swaps `memory[0]` for the new system message and records a `prompt`
    /// session event, so a resume rebuilds exactly this prompt
    /// (`Session::replay` picks the last `prompt` event over the composed
    /// default; sessions without `prompt` events replay unchanged).
    pub fn update_system_prompt(&mut self, text: &str) {
        let msg = json!({ "role": "system", "content": text });
        if self
            .memory
            .first()
            .is_some_and(|m| m.get("role").and_then(Value::as_str) == Some("system"))
        {
            self.memory[0] = msg;
        } else {
            self.memory.insert(0, msg);
        }
        self.session.record_prompt(text);
    }

    // -- one turn -----------------------------------------------------------

    /// Drive one full turn (port of `run_turn`).
    pub async fn run_turn(&mut self, user_message: &str) -> TurnResult {
        let turn_n = self.session.turn_number() + 1;
        self.cancel_clear();

        // 1. user message -> memory + JSONL
        self.session.record_user(user_message);
        self
            .memory
            .push(json!({ "role": "user", "content": user_message }));

        // 2. budget check -> compaction pass
        if let Some(fail) = self.check_budget(turn_n, None).await {
            return fail;
        }

        let max_rounds = self.cfg.tools.limits.max_tool_rounds;
        let mut rounds: u32 = 0;
        let mut malformed_streak: u32 = 0;
        let mut prompt_tokens = self.session.last_prompt_tokens();
        let mut completion_tokens: u64 = 0;
        let turn_started = Instant::now();

        loop {
            // Everything currently in memory goes out on this request; the
            // server's prompt count for it is the authoritative baseline
            // for the budget gate until the next request appends more.
            self.requested_mem_len = self.memory.len();
            // 3. one streaming request. Cancel stops iteration and the
            //    partial output is simply never appended (the loop discards
            //    the dropped stream — Python: Esc -> resp.close() in
            //    finally). stream() folds live backend usage updates into
            //    prompt_tokens/completion_tokens as they arrive.
            let completion_base = completion_tokens;
            let streamed = tokio::select! {
                r = self.stream(&mut prompt_tokens, &mut completion_tokens) => r,
                _ = self.wait_cancel() => Ok(StreamResult {
                    content: String::new(),
                    reasoning: String::new(),
                    tool_calls: Vec::new(),
                    usage: None,
                }),
            };
            let StreamResult {
                content,
                reasoning,
                tool_calls,
                usage,
            } = match streamed {
                Err(e) => {
                    let msg = e.to_string();
                    // Richer context than the bare error: the backend
                    // message ("error decoding response body") is opaque
                    // without knowing what we were doing when it died.
                    let ctx = format!(
                        "{msg} (model={} · round={} · memory_messages={} · \
                         prompt_tokens≈{} · turn {}s in)",
                        self.client.model(),
                        rounds,
                        self.memory.len(),
                        prompt_tokens,
                        turn_started.elapsed().as_secs(),
                    );
                    crate::logmsg::warn(format!("[backend] stream failed: {ctx}"));
                    self.session.record_error(&ctx);
                    (self.emit)(&"error", &json!({ "text": ctx }));
                    self.wrapup(turn_n, prompt_tokens, completion_tokens, "aborted: backend error", false);
                    return TurnResult::abort(&format!("backend error: {msg}"), prompt_tokens, completion_tokens);
                }
                Ok(v) => v,
            };
            if self.cancel_flag() {
                return self.finish_interrupted(
                    turn_n, &[], prompt_tokens, completion_tokens,
                    "interrupted — partial output discarded",
                );
            }

            if let Some(u) = usage {
                // Reconcile to the authoritative per-request totals. The
                // live SseEvent::Usage updates already advanced the
                // counters to (at most) these values, so for a fully
                // received stream this is a no-op; it matters when the
                // stream ended without a terminal usage chunk carrying
                // the final counts.
                if u.prompt_tokens > 0 {
                    prompt_tokens = prompt_tokens.max(u.prompt_tokens);
                }
                completion_tokens = completion_base + u.completion_tokens;
                // Did this request give us a server-side prompt count?
                // This is what the budget gate trusts on the next pass.
                self.live_usage = u.prompt_tokens > 0;
            } else {
                // backend reported neither usage nor timings: estimate
                self.live_usage = false;
                prompt_tokens = estimate_prompt_tokens(&self.memory);
            }
            // Live counts for the UI (CR_01/5): the status bar updates
            // mid-turn (stream() emits per live usage update) and this
            // closes each request out, including the estimate fallback.
            (self.emit)(&"usage", &json!({
                "prompt": prompt_tokens,
                "completion": completion_tokens,
            }));

            // 5. no tool calls -> final assistant message -> wrap-up
            if tool_calls.is_empty() {
                let msg = assistant_msg(&content, &reasoning, None);
                self.memory.push(msg.clone());
                self.session.record_assistant(
                    &msg["content"],
                    None,
                    msg.get("reasoning_content").and_then(Value::as_str),
                );
                let commit = self.wrapup(
                    turn_n,
                    prompt_tokens,
                    completion_tokens,
                    &first_line(&content, 60),
                    true,
                );
                return TurnResult {
                    ok: true,
                    reason: String::new(),
                    commit,
                    prompt_tokens,
                    completion_tokens,
                };
            }

            // guard: max tool rounds per turn. The dangling
            // assistant+tool_calls message is deliberately NOT appended, so
            // memory stays valid.
            if rounds >= max_rounds {
                self.session
                    .record_turn_aborted(&format!("max_tool_rounds ({max_rounds}) exceeded"));
                (self.emit)(&"error", &json!({
                    "text": format!("max tool rounds ({max_rounds}) exceeded — stopping")
                }));
                self.wrapup(turn_n, prompt_tokens, completion_tokens, "aborted: max tool rounds", false);
                return TurnResult::abort(
                    &format!("max tool rounds ({max_rounds}) exceeded"),
                    prompt_tokens,
                    completion_tokens,
                );
            }
            rounds += 1;

            // 4. assistant message with tool_calls -> memory + JSONL, then
            //    execute
            self.memory.push(assistant_msg(
                &content,
                &reasoning,
                Some(&json!(tool_calls)),
            ));
            self.session.record_assistant(
                &Value::String(content.clone()),
                Some(&json!(tool_calls)),
                (!reasoning.is_empty()).then(|| reasoning.as_str()),
            );

            let mut stop_reason = None;
            for tc in &tool_calls {
                let (streak, abort) = self.execute_call(tc, malformed_streak).await;
                malformed_streak = streak;
                if abort.is_some() {
                    stop_reason = abort;
                    break;
                }
            }
            if let Some(reason) = stop_reason {
                self.wrapup(turn_n, prompt_tokens, completion_tokens, "aborted: malformed tool calls", false);
                return TurnResult::abort(&reason, prompt_tokens, completion_tokens);
            }

            if self.cancel_flag() {
                return self.finish_interrupted(
                    turn_n, &tool_calls, prompt_tokens, completion_tokens,
                    "interrupted during tool execution",
                );
            }

            // 6. mid-turn budget check: a long tool chain grows memory every
            //    round, so compact inside the loop too, not just at turn
            //    start. Memory is always valid here: the
            //    assistant+tool_calls+tool group from this round is
            //    complete. prompt_tokens is the live in-turn count (backend
            //    usage when available), so growth from this turn's tool
            //    results actually trips the threshold.
            if let Some(fail) = self.check_budget(turn_n, Some(prompt_tokens)).await {
                return fail;
            }

            // otherwise: loop back to 3 — request-driven, approval pauses
            // cost nothing on the wire
        }
    }

    // -- interruption ---------------------------------------------------------

    /// Backfill + interrupted event + wrap-up; returns the interrupted
    /// TurnResult. `tool_calls` is the current batch (empty when the
    /// interruption hit between batches).
    fn finish_interrupted(
        &mut self,
        turn_n: u64,
        tool_calls: &[Value],
        prompt_tokens: u64,
        completion_tokens: u64,
        message: &str,
    ) -> TurnResult {
        self.backfill_interrupted(tool_calls);
        self.session.record_interrupted();
        (self.emit)(&"info", &json!({ "text": message }));
        self.wrapup(turn_n, prompt_tokens, completion_tokens, "interrupted", false);
        TurnResult::abort("interrupted", prompt_tokens, completion_tokens)
    }

    /// Auto compaction pass. Returns None when memory fits the budget (or
    /// summarization is off) — otherwise a `TurnResult` that aborts the
    /// turn. Called at turn start AND after each tool round, so a long tool
    /// chain compacts consistently instead of only at the next turn's
    /// start.
    ///
    /// The gate is driven by the SERVER'S live count when the backend
    /// reports usage (the refactor away from internal counting): the last
    /// reported `prompt_tokens` covers exactly `memory[..requested_mem_len]`
    /// as the server tokenized it, and only the tail appended since that
    /// request (the current round's assistant+tool group, or the new user
    /// message) carries a local chars/4 estimate. That matters because the
    /// estimate can overcount what the server tokenizes (repetitive /
    /// high-entropy content); the old `max(live, whole_memory_estimate)`
    /// let the estimate alone force premature compaction. Without live data
    /// (fresh session, or a backend that reports no usage) the whole-memory
    /// estimate is the only signal and remains the gate.
    ///
    /// Over-gating is safe — `ensure_budget` re-checks with its own
    /// estimate and is a no-op when memory already fits.
    async fn check_budget(&mut self, turn_n: u64, known_tokens: Option<u64>) -> Option<TurnResult> {
        if self.cfg.context.summarize.as_str() != "auto" {
            return None;
        }
        let live_base = known_tokens
            .unwrap_or(0)
            .max(self.session.last_prompt_tokens());
        let known = if self.live_usage {
            // live count of what was last sent + estimate of the unsent
            // tail. Memory can be shorter than `requested_mem_len` right
            // after a compaction shrank it — the tail is then empty and
            // the stale base is an over-gate at worst.
            let from = self.requested_mem_len.min(self.memory.len());
            live_base + estimate_prompt_tokens(&self.memory[from..])
        } else {
            live_base.max(estimate_prompt_tokens(&self.memory))
        };
        let threshold = self.cfg.context.threshold_tokens();
        if known <= threshold {
            return None;
        }
        let cancel_rx = self.cancel_rx.clone();
        let result = ensure_budget(
            &self.memory,
            self.client.as_ref(),
            &self.cfg,
            &mut self.session,
            Some(&|| *cancel_rx.borrow()),
            &DefaultFindCut,
        )
        .await;
        match result {
            Err(e) => {
                let msg = e.to_string();
                self.session.record_error(&format!("compaction failed: {msg}"));
                (self.emit)(&"error", &json!({ "text": format!("compaction failed: {msg}") }));
                self.wrapup(turn_n, known, 0, "aborted: compaction failed", false);
                Some(TurnResult::abort(&format!("compaction failed: {msg}"), known, 0))
            }
            Ok(None) => {
                // error event already recorded by ensure_budget
                self.wrapup(turn_n, known, 0, "aborted: context exceeded", false);
                Some(TurnResult::abort("context exceeded, start a new session", known, 0))
            }
            Ok(Some(new_mem)) => {
                if new_mem != self.memory {
                    self.memory = new_mem;
                    (self.emit)(&"info", &json!({ "text": "context compacted" }));
                }
                None
            }
        }
    }

    // -- internals ----------------------------------------------------------

    /// Consume one streamed response. `Err(BackendError)` aborts the turn;
    /// cancel stops iteration and the partial accumulation is discarded by
    /// the caller (Python: the
    /// `KeyboardInterrupt` path).
    ///
    /// `prompt_tokens` / `completion_tokens` are the TURN-level running
    /// counters (prompt = context size, completion = summed over every
    /// request in the turn). Backend `SseEvent::Usage` updates flow into
    /// them LIVE, mid-stream: the prompt counter takes the max (context
    /// only grows), the completion counter tracks this request's running
    /// total on top of the turn's pre-request base.
    async fn stream(
        &self,
        prompt_tokens: &mut u64,
        completion_tokens: &mut u64,
    ) -> Result<StreamResult, BackendError> {
        let mut content_parts: Vec<String> = Vec::new();
        let mut reasoning_parts: Vec<String> = Vec::new();
        // index -> (id, name, argument fragments), sorted by index like
        // Python's `sorted(calls)`.
        let mut calls: std::collections::BTreeMap<u64, (Option<String>, Option<String>, Vec<String>)> =
            std::collections::BTreeMap::new();
        let mut usage: Option<Usage> = None;
        // Live counters from the backend (per-chunk `timings` / `usage`
        // objects): completion is a per-request running total, so track
        // its max and offset it on the turn's pre-request base.
        let completion_base = *completion_tokens;
        let mut completion_seen: u64 = 0;
        let mut last_live: Option<(u64, u64)> = None;
        // Idle-stall diagnostics: if the backend stops sending bytes for a
        // long time the stream looks "fine" but is wedged (previously such
        // stalls were the hard part of diagnosing mid-stream errors). The
        // client's read_timeout eventually fails it; until then, log a
        // heartbeat once per minute of silence.
        let mut last_chunk = Instant::now();
        let mut last_warn = last_chunk;

        let mut stream = self
            .client
            .stream_chat(&self.memory, Some(&self.schemas))
            .await?;
        loop {
            let item = tokio::select! {
                item = stream.next() => item,
                _ = self.wait_cancel() => None,
            };
            let Some(item) = item else { break };
            let now = Instant::now();
            let got_data = matches!(item, Ok(StreamItem::Event(_)));
            if got_data {
                if now.duration_since(last_warn) >= Duration::from_secs(60)
                    && now.duration_since(last_chunk) < Duration::from_secs(60)
                {
                    crate::logmsg::warn(format!(
                        "[backend] stream resumed after {:.0}s of silence",
                        now.duration_since(last_chunk).as_secs_f64()
                    ));
                }
                last_warn = now;
                last_chunk = now;
            } else if last_chunk.elapsed() >= Duration::from_secs(60)
                && now.duration_since(last_warn) >= Duration::from_secs(60)
            {
                crate::logmsg::warn(format!(
                    "[backend] no data from model for {:.0}s — waiting for the idle timeout ({:?})",
                    last_chunk.elapsed().as_secs_f64(),
                    crate::backend::client::READ_TIMEOUT
                ));
                last_warn = now;
            }
            match item {
                Ok(StreamItem::Event(ev)) => match ev {
                    SseEvent::Reasoning(text) => {
                        reasoning_parts.push(text.clone());
                        (self.emit)(&"reasoning", &json!({ "text": text }));
                    }
                    SseEvent::Text(text) => {
                        content_parts.push(text.clone());
                        (self.emit)(&"text", &json!({ "text": text }));
                    }
                    SseEvent::Usage(u) => {
                        // Live backend counters (this request's running
                        // totals): fold them into the turn counters and
                        // push a `usage` UI event — but only when
                        // something changed, so a chatty per-token
                        // `timings` stream doesn't spam the UI channel.
                        if u.prompt_tokens > 0 {
                            *prompt_tokens = (*prompt_tokens).max(u.prompt_tokens);
                        }
                        completion_seen = completion_seen.max(u.completion_tokens);
                        *completion_tokens = completion_base + completion_seen;
                        if last_live != Some((*prompt_tokens, *completion_tokens)) {
                            last_live = Some((*prompt_tokens, *completion_tokens));
                            (self.emit)(&"usage", &json!({
                                "prompt": *prompt_tokens,
                                "completion": *completion_tokens,
                            }));
                        }
                    }
                    SseEvent::ToolCall {
                        index,
                        id,
                        name,
                        arguments,
                    } => {
                        let c = calls.entry(index).or_insert((None, None, Vec::new()));
                        if id.is_some() {
                            c.0 = id;
                        }
                        if name.is_some() {
                            c.1 = name;
                        }
                        if !arguments.is_empty() {
                            c.2.push(arguments);
                        }
                    }
                },
                Ok(StreamItem::Done { usage: u, .. }) => {
                    usage = u;
                    break;
                }
                Err(e) => return Err(e),
            }
        }

        let mut tool_calls: Vec<Value> = Vec::new();
        for (_idx, (id, name, args)) in calls {
            tool_calls.push(json!({
                "id": id.unwrap_or_else(|| format!("call_{}", tool_calls.len())),
                "type": "function",
                "function": {
                    "name": name.unwrap_or_else(|| "unknown".into()),
                    "arguments": args.join(""),
                },
            }));
        }
        Ok(StreamResult {
            content: content_parts.concat(),
            reasoning: reasoning_parts.concat(),
            tool_calls,
            usage,
        })
    }

    /// One tool call: parse -> disabled? -> approval? -> execute -> record.
    /// Returns (new_malformed_streak, abort_reason_or_None).
    async fn execute_call(&mut self, tc: &Value, mut malformed_streak: u32) -> (u32, Option<String>) {
        let name = tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let call_id = tc
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let raw_args = tc
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // malformed tool JSON is data for the model; 2 consecutive aborts
        let args: Value;
        match serde_json::from_str::<Value>(&raw_args) {
            Ok(v) if v.is_object() => {
                args = v;
                malformed_streak = 0;
            }
            _ => {
                malformed_streak += 1;
                self.record_tool(
                    &call_id,
                    &name,
                    &json!(raw_args),
                    ToolResult::err(format!(
                        "error: malformed tool call JSON: {}",
                        &raw_args[..raw_args.len().min(200)]
                    )),
                );
                if malformed_streak >= 2 {
                    self.session
                        .record_turn_aborted("two consecutive malformed tool calls");
                    (self.emit)(&"error", &json!({
                        "text": "two consecutive malformed tool calls — stopping"
                    }));
                    return (
                        malformed_streak,
                        Some("two consecutive malformed tool calls".into()),
                    );
                }
                return (malformed_streak, None);
            }
        }

        if !self.enabled.contains(&name) {
            self.record_tool(
                &call_id,
                &name,
                &args,
                ToolResult::err(format!("error: tool {name} is disabled")),
            );
            return (malformed_streak, None);
        }

        if registry::approval_for(&name, &self.cfg) == "ask"
            && !self.always_allow.contains(&name)
        {
            let preview = make_preview(&name, &args, &self.ctx.workspace);
            let decision = (self.ask)(&name, &args, &preview);
            self.session.record_approval(&name, &decision);
            if decision == "a" {
                self.always_allow.insert(name.clone());
            }
            if decision != "y" && decision != "a" {
                self.record_tool(&call_id, &name, &args, ToolResult::err("denied by user"));
                return (malformed_streak, None);
            }
        }

        (self.emit)(&"tool", &json!({ "name": name, "args": args }));
        let result = registry::execute(&name, args.clone(), &self.ctx).await;
        self.record_tool(&call_id, &name, &args, result);
        (malformed_streak, None)
    }

    fn record_tool(&mut self, call_id: &str, name: &str, args: &Value, result: ToolResult) {
        self.session
            .record_tool(call_id, name, args, &result.result, result.ok);
        // Vision tool results (CR_01/8): the wire message carries text +
        // image_url parts; the JSONL audit log keeps the text only (the
        // images are not persisted — a resumed session re-reads the file
        // via read_file when it needs them again).
        let content = if result.images.is_empty() {
            Value::String(result.result.clone())
        } else {
            json!([{ "type": "text", "text": result.result },
                   { "type": "image_url", "image_url": { "url": result.images[0] } }])
        };
        self.memory.push(json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": content,
        }));
        (self.emit)(&"tool_result", &json!({
            "name": name,
            "ok": result.ok,
            "result": result.result,
        }));
    }

    /// If the turn died mid-batch, give every unexecuted call an error
    /// result so the assistant+tool_calls+tool group stays complete.
    fn backfill_interrupted(&mut self, tool_calls: &[Value]) {
        let recorded: std::collections::HashSet<String> = self
            .session
            .events()
            .iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some("tool"))
            .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        for tc in tool_calls {
            let id = tc.get("id").and_then(Value::as_str).unwrap_or_default();
            if recorded.iter().any(|r| r == id) {
                continue;
            }
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .cloned()
                .unwrap_or(Value::Null);
            self.record_tool(
                id,
                name,
                &args,
                ToolResult::err("error: interrupted before execution"),
            );
        }
    }

    /// Print the workspace diff for this turn (port of `_show_diff`).
    fn show_diff(&self, ws: &std::path::Path) {
        let mut parts: Vec<String> = Vec::new();
        let new_files = git_out(&["ls-files", "--others", "--exclude-standard"], ws).trim().to_string();
        if !new_files.is_empty() {
            parts.push(format!("new files:\n{new_files}"));
        }
        let diff = git_out(&["diff", "HEAD"], ws).trim().to_string();
        if !diff.is_empty() {
            parts.push(diff);
        }
        (self.emit)(&"info", &json!({
            "text": format!(
                "workspace changes this turn:\n{}",
                short_text(&parts.join("\n\n"), 4000)
            )
        }));
    }

    /// Final diff + the two independent auto-commits. Returns the
    /// workspace commit hash (None when the workspace was unchanged).
    fn wrapup(
        &mut self,
        turn_n: u64,
        prompt_tokens: u64,
        completion_tokens: u64,
        summary: &str,
        completed: bool,
    ) -> Option<String> {
        let mut commit: Option<String> = None;
        if self.cfg.workspace.auto_git {
            let ws = std::path::Path::new(&self.cfg.workspace.path);
            if !git_out(&["status", "--porcelain"], ws).trim().is_empty() {
                self.show_diff(ws);
                let msg = format!(
                    "little_helper: {} {summary} [session:{}]",
                    self.session.agent(),
                    self.session.id()
                );
                commit = gitstore::commit_all(ws, &msg);
            }
        }
        if completed {
            self.session
                .record_turn_end(turn_n, commit.as_deref(), prompt_tokens, completion_tokens);
        }
        let state_msg = format!(
            "session {}: turn {}{}",
            self.session.id(),
            turn_n,
            if completed { "" } else { " (aborted)" }
        );
        gitstore::commit_all(&config::state_dir(), &state_msg);
        let mut bits = vec![
            format!("turn {turn_n} {}", if completed { "done" } else { "aborted" }),
            format!("prompt_tokens={prompt_tokens}"),
            format!("completion_tokens={completion_tokens}"),
        ];
        if let Some(c) = &commit {
            bits.push(format!("workspace@{c}"));
        }
        (self.emit)(&"info", &json!({ "text": bits.join("  ") }));
        commit
    }

    /// Manual compaction (summarize = "manual"; the TUI will call this).
    pub async fn compact_now(&mut self) -> bool {
        let result = ensure_budget(
            &self.memory,
            self.client.as_ref(),
            &self.cfg,
            &mut self.session,
            Some(&|| false),
            &DefaultFindCut,
        )
        .await;
        match result {
            Err(e) => {
                (self.emit)(&"error", &json!({ "text": format!("compaction failed: {e}") }));
                false
            }
            Ok(None) => {
                // error event already recorded ("context exceeded...")
                (self.emit)(&"info", &json!({ "text": "nothing to compact" }));
                false
            }
            Ok(Some(new_mem)) if new_mem == self.memory => {
                (self.emit)(&"info", &json!({ "text": "nothing to compact" }));
                false
            }
            Ok(Some(new_mem)) => {
                self.memory = new_mem;
                (self.emit)(&"info", &json!({ "text": "context compacted" }));
                true
            }
        }
    }
}

// Note on the cancel flag: Python's `AgentLoop` reuses one `threading.Event`
// and `clear()`s it at the start of every `run_turn`. The watch bool above
// mirrors that one-for-one (`send(true)` == `Event.set()`, `send(false)` ==
// `Event.clear()`, `cancel_flag()` == `Event.is_set()`), so the same
// semantics hold without a `CancellationToken` (which has no reset).

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{CompleteFut, ChatStream};
    use crate::config::{ContextConfig, WorkspaceConfig};
    use futures_util::stream;

    struct NoopBackend;
    impl Backend for NoopBackend {
        fn stream_chat<'a>(&'a self, _m: &'a [Value], _t: Option<&'a [Value]>) -> ChatStream<'a> {
            Box::pin(async {
                Ok(Box::pin(stream::once(async {
                    Ok::<_, BackendError>(StreamItem::Done {
                        finish_reason: "stop".into(),
                        usage: None,
                    })
                })) as _)
            })
        }
        fn complete<'a>(&'a self, _m: &'a [Value], _t: u64) -> CompleteFut<'a> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    // Per-test isolation: a process-wide counter, so concurrent tests never
    // share a workspace or session dir (nanosecond-only uniqueness let two
    // tests collide into the same .jsonl — see the TOCTOU in Session::create).
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn seq() -> usize {
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn tmp_cfg() -> Config {
        crate::tools::register_all();
        let ws = std::env::temp_dir().join(format!(
            "harness-loop-ws-{}-{}",
            std::process::id(),
            seq()
        ));
        let _ = std::fs::create_dir_all(&ws);
        Config {
            context: ContextConfig {
                summarize_threshold: 0.7,
                ..ContextConfig::default()
            },
            workspace: WorkspaceConfig {
                path: ws.to_string_lossy().into_owned(),
                auto_git: false,
            },
            ..Config::default()
        }
    }

    fn tmp_session() -> Session {
        let dir = std::env::temp_dir().join(format!(
            "harness-loop-sess-{}-{}",
            std::process::id(),
            seq()
        ));
        let _ = std::fs::create_dir_all(&dir);
        Session::create("coder", "m", Some(&dir))
    }

    #[tokio::test]
    async fn simple_text_turn_completes() {
        let cfg = tmp_cfg();
        let session = tmp_session();
        let mut loop_ = AgentLoop::new(
            cfg,
            Arc::new(NoopBackend),
            session,
            Some(Arc::new(|_, _, _| "y".to_string())),
            Some(Arc::new(|_, _| {})),
        );
        let res = loop_.run_turn("hello").await;
        assert!(res.ok, "{}", res.reason);
        assert_eq!(loop_.session.turn_number(), 1);
    }

    #[test]
    fn first_line_truncates_and_defaults() {
        assert_eq!(first_line("short line\nsecond", 60), "short line");
        assert_eq!(first_line("", 60), "(no summary)");
        assert_eq!(first_line("\n\n  \n", 60), "(no summary)");
        assert_eq!(
            first_line(&"a".repeat(100), 60),
            format!("{}…", "a".repeat(59))
        );
    }

    #[test]
    fn make_preview_shapes() {
        let ws = std::path::PathBuf::from("/tmp");
        assert_eq!(make_preview("run_command", &json!({"command": "ls"}), &ws), "$ ls");
        assert_eq!(make_preview("web_fetch", &json!({"url": "https://x"}), &ws), "GET https://x");
        assert_eq!(
            make_preview("web_search", &json!({"query": "rust"}), &ws),
            "search: rust"
        );
        assert_eq!(
            make_preview("delete_file", &json!({"path": "a.txt"}), &ws),
            "delete a.txt"
        );
        let p = make_preview("write_file", &json!({"path": "a.txt", "content": "hi"}), &ws);
        assert!(p.starts_with("a.txt\nhi"), "{p}");
    }

    #[test]
    fn short_text_prefers_line_boundary() {
        let text = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let s = short_text(&text, 60);
        assert!(s.ends_with("…[truncated at 60 chars]"), "{s}");
        assert!(!s.trim_end().ends_with(' '));
        assert_eq!(short_text("short", 100), "short");
    }

    // ---------- phase 5: update_system_prompt (plan §4.2) ----------

    #[test]
    fn update_system_prompt_replaces_memory0_and_records_event() {
        let cfg = tmp_cfg();
        let session = tmp_session();
        let mut loop_ = AgentLoop::new(
            cfg,
            Arc::new(NoopBackend),
            session,
            Some(Arc::new(|_, _, _| "y".to_string())),
            Some(Arc::new(|_, _| {})),
        );
        let default_sys = loop_.memory[0].clone();
        assert_eq!(default_sys["role"], "system");

        loop_.update_system_prompt("NEW SYSTEM PROMPT");
        assert_eq!(loop_.memory[0]["role"], "system");
        assert_eq!(loop_.memory[0]["content"], "NEW SYSTEM PROMPT");
        assert_ne!(loop_.memory[0], default_sys);
        // exactly one prompt event, appended last
        let prompts: Vec<&Value> = loop_
            .session
            .events()
            .iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some("prompt"))
            .collect();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0]["text"], "NEW SYSTEM PROMPT");
        // live memory still matches replay (sans system, which is the
        // overridden prompt on both sides)
        let replayed =
            loop_.session.replay(default_sys["content"].as_str().unwrap_or_default());
        assert_eq!(loop_.memory, replayed);
        // second call: last prompt wins, memory[0] stays a single system msg
        loop_.update_system_prompt("NEWER");
        assert_eq!(loop_.memory[0]["content"], "NEWER");
        assert_eq!(
            loop_
                .memory
                .iter()
                .filter(|m| m["role"] == "system")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn update_system_prompt_survives_resume() {
        let cfg = tmp_cfg();
        let session = tmp_session();
        let session_path = session.path.clone();
        let mut loop_ = AgentLoop::new(
            cfg,
            Arc::new(NoopBackend),
            session,
            Some(Arc::new(|_, _, _| "y".to_string())),
            Some(Arc::new(|_, _| {})),
        );
        let res = loop_.run_turn("hello").await;
        assert!(res.ok, "{}", res.reason);
        loop_.update_system_prompt("RESUME SYSTEM");

        // A fresh AgentLoop resumed from disk must carry the override —
        // even with a workspace whose composed default differs.
        let reopened = Session::open(&session_path);
        let mut resumed = AgentLoop::new(
            tmp_cfg(),
            Arc::new(NoopBackend),
            reopened,
            Some(Arc::new(|_, _, _| "y".to_string())),
            Some(Arc::new(|_, _| {})),
        );
        assert_eq!(resumed.memory[0]["content"], "RESUME SYSTEM");
        // the resumed loop can override again; the newest event wins
        resumed.update_system_prompt("RESUME SYSTEM 2");
        assert_eq!(
            Session::open(&session_path)
                .replay("DEFAULT")
                .first()
                .unwrap()["content"],
            "RESUME SYSTEM 2"
        );
    }

    #[tokio::test]
    async fn old_logs_without_prompt_events_replay_unchanged() {
        // Backward compat (plan §4.2): a session written without any
        // `prompt` event replays with the composed default system message —
        // exactly as the pre-phase-5 code did.
        let cfg = tmp_cfg();
        let session = tmp_session();
        let mut loop_ = AgentLoop::new(
            cfg,
            Arc::new(NoopBackend),
            session,
            Some(Arc::new(|_, _, _| "y".to_string())),
            Some(Arc::new(|_, _| {})),
        );
        let composed = loop_.memory[0].clone();
        let res = loop_.run_turn("hi").await;
        assert!(res.ok, "{}", res.reason);
        let session_path = loop_.session.path.clone();
        assert!(loop_
            .session
            .events()
            .iter()
            .all(|e| e.get("type").and_then(Value::as_str) != Some("prompt")));
        let reopened = Session::open(&session_path);
        let mem = reopened.replay(&prompts::system_prompt(&reopened.agent(), &loop_.ctx.workspace));
        assert_eq!(mem.first().unwrap(), &composed);
        // and replaying with a sentinel default still honors the sentinel
        // (no prompt event to override it)
        let mem = reopened.replay("SENTINEL");
        assert_eq!(mem[0]["content"], "SENTINEL");
    }

    #[tokio::test]
    async fn cancel_stops_an_in_flight_stream() {
        use crate::backend::SseEvent;

        /// Emits one text delta, then parks at Pending forever — the turn
        /// must end via cancel, not via stream end.
        struct SlowStream {
            sent: bool,
        }
        impl futures_util::Stream for SlowStream {
            type Item = Result<StreamItem, BackendError>;
            fn poll_next(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                if !self.sent {
                    self.sent = true;
                    return std::task::Poll::Ready(Some(Ok(StreamItem::Event(
                        SseEvent::Text("part".into()),
                    ))));
                }
                std::task::Poll::Pending
            }
        }

        struct SlowBackend;
        impl Backend for SlowBackend {
            fn stream_chat<'a>(&'a self, _m: &'a [Value], _t: Option<&'a [Value]>) -> ChatStream<'a> {
                Box::pin(async {
                    Ok(Box::pin(SlowStream { sent: false }) as _)
                })
            }
            fn complete<'a>(&'a self, _m: &'a [Value], _t: u64) -> CompleteFut<'a> {
                Box::pin(async { Ok(String::new()) })
            }
        }

        let cfg = tmp_cfg();
        let session = tmp_session();
        let mut loop_ = AgentLoop::new(
            cfg,
            Arc::new(SlowBackend),
            session,
            Some(Arc::new(|_, _, _| "y".to_string())),
            Some(Arc::new(|_, _| {})),
        );
        let handle = tokio::spawn({
            let cancel = loop_.cancel_tx.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let _ = cancel.send(true);
            }
        });
        let res = loop_.run_turn("stream forever").await;
        handle.await.unwrap();
        assert!(!res.ok);
        assert_eq!(res.reason, "interrupted");
        // interrupted event recorded
        assert!(loop_
            .session
            .events()
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("interrupted")));
    }

    // ---------- CR_01: vision tool results + live usage events ----------

    #[tokio::test]
    async fn usage_event_emitted_with_backend_counts() {
        use crate::backend::SseEvent;

        /// One stream: a text delta + Done with explicit usage.
        struct UsageStream {
            sent: bool,
        }
        impl futures_util::Stream for UsageStream {
            type Item = Result<StreamItem, BackendError>;
            fn poll_next(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                if !self.sent {
                    self.sent = true;
                    return std::task::Poll::Ready(Some(Ok(StreamItem::Event(
                        SseEvent::Text("hi".into()),
                    ))));
                }
                std::task::Poll::Ready(Some(Ok(StreamItem::Done {
                    finish_reason: "stop".into(),
                    usage: Some(crate::backend::Usage {
                        prompt_tokens: 123,
                        completion_tokens: 7,
                    }),
                })))
            }
        }

        struct UsageBackend;
        impl Backend for UsageBackend {
            fn stream_chat<'a>(&'a self, _m: &'a [Value], _t: Option<&'a [Value]>) -> ChatStream<'a> {
                Box::pin(async {
                    Ok(Box::pin(UsageStream { sent: false }) as _)
                })
            }
            fn complete<'a>(&'a self, _m: &'a [Value], _t: u64) -> CompleteFut<'a> {
                Box::pin(async { Ok(String::new()) })
            }
        }

        let cfg = tmp_cfg();
        let session = tmp_session();
        let kinds: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let kinds2 = kinds.clone();
        let emit = Arc::new(move |kind: &str, _d: &Value| {
            kinds2.lock().unwrap().push(kind.to_string());
        });
        let mut loop_ = AgentLoop::new(
            cfg,
            Arc::new(UsageBackend),
            session,
            Some(Arc::new(|_, _, _| "y".to_string())),
            Some(emit),
        );
        let res = loop_.run_turn("hi").await;
        assert!(res.ok, "{}", res.reason);
        // the usage event was emitted with the backend's own counts
        assert!(kinds.lock().unwrap().iter().any(|k| k == "usage"),
            "no usage event emitted: {:?}", kinds.lock().unwrap());
    }

    /// Live per-chunk counters (llama.cpp `timings` shape): running totals
    /// for the CURRENT request must flow into the turn counters live —
    /// and must NOT be double-counted when the terminal Done carries the
    /// same final totals.
    #[tokio::test]
    async fn live_usage_events_feed_turn_counters_without_double_counting() {
        use crate::backend::SseEvent;

        struct LiveStream {
            n: u8,
        }
        impl futures_util::Stream for LiveStream {
            type Item = Result<StreamItem, BackendError>;
            fn poll_next(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                use crate::backend::Usage;
                let ev = |u: (u64, u64)| {
                    Ok(StreamItem::Event(SseEvent::Usage(Usage {
                        prompt_tokens: u.0,
                        completion_tokens: u.1,
                    })))
                };
                match self.n {
                    0 => {
                        self.n = 1;
                        std::task::Poll::Ready(Some(ev((100, 1))))
                    }
                    1 => {
                        self.n = 2;
                        std::task::Poll::Ready(Some(Ok(StreamItem::Event(
                            SseEvent::Text("hi".into()),
                        ))))
                    }
                    2 => {
                        self.n = 3;
                        std::task::Poll::Ready(Some(ev((100, 5))))
                    }
                    _ => std::task::Poll::Ready(Some(Ok(StreamItem::Done {
                        finish_reason: "stop".into(),
                        usage: Some(crate::backend::Usage {
                            prompt_tokens: 100,
                            completion_tokens: 5,
                        }),
                    }))),
                }
            }
        }

        struct LiveBackend;
        impl Backend for LiveBackend {
            fn stream_chat<'a>(&'a self, _m: &'a [Value], _t: Option<&'a [Value]>) -> ChatStream<'a> {
                Box::pin(async {
                    Ok(Box::pin(LiveStream { n: 0 }) as _)
                })
            }
            fn complete<'a>(&'a self, _m: &'a [Value], _t: u64) -> CompleteFut<'a> {
                Box::pin(async { Ok(String::new()) })
            }
        }

        let cfg = tmp_cfg();
        let session = tmp_session();
        let events: Arc<std::sync::Mutex<Vec<(String, Value)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let events2 = events.clone();
        let emit = Arc::new(move |kind: &str, data: &Value| {
            events2.lock().unwrap().push((kind.to_string(), data.clone()));
        });
        let mut loop_ = AgentLoop::new(
            cfg,
            Arc::new(LiveBackend),
            session,
            Some(Arc::new(|_, _, _| "y".to_string())),
            Some(emit),
        );
        let res = loop_.run_turn("hi").await;
        assert!(res.ok, "{}", res.reason);
        // turn totals come from the backend, not the chars/4 estimate —
        // and the terminal Done's totals are not added on top of the
        // live updates (5, not 1+5).
        assert_eq!(res.prompt_tokens, 100);
        assert_eq!(res.completion_tokens, 5);
        // the first live usage event arrives BEFORE the text — that is
        // the whole point of the streamed counters.
        let got = events.lock().unwrap();
        let first_usage = got
            .iter()
            .position(|(k, _)| k == "usage")
            .expect("no usage event");
        let first_text = got
            .iter()
            .position(|(k, _)| k == "text")
            .expect("no text event");
        assert!(first_usage < first_text, "usage not live: {:?}", got);
        let live_values: Vec<u64> = got
            .iter()
            .filter(|(k, _)| k == "usage")
            .map(|(_, d)| d["completion"].as_u64().unwrap())
            .collect();
        // live 1 -> live 5 -> request-end reconcile 5 (dedup keeps it once)
        assert_eq!(live_values, vec![1, 5, 5]);
    }

    /// The budget gate is driven by the server's live count, not the local
    /// chars/4 estimate: a whole-memory estimate ABOVE the threshold must
    /// NOT trigger compaction while the live count plus the unsent tail
    /// stay BELOW it (the old `max(live, whole_estimate)` gate fired here).
    #[tokio::test]
    async fn budget_gate_prefers_live_count_over_estimate() {
        /// Always reports the same small prompt count + one text token.
        struct SmallLive {
            prompt: u64,
            complete_calls: std::sync::atomic::AtomicUsize,
        }
        impl Backend for SmallLive {
            fn stream_chat<'a>(&'a self, _m: &'a [Value], _t: Option<&'a [Value]>) -> ChatStream<'a> {
                let prompt = self.prompt;
                Box::pin(async move {
                    Ok(Box::pin(stream::iter(vec![
                        Ok(StreamItem::Event(SseEvent::Text("ok".into()))),
                        Ok(StreamItem::Done {
                            finish_reason: "stop".into(),
                            usage: Some(Usage {
                                prompt_tokens: prompt,
                                completion_tokens: 1,
                            }),
                        }),
                    ])) as _)
                })
            }
            fn complete<'a>(&'a self, _m: &'a [Value], _t: u64) -> CompleteFut<'a> {
                self.complete_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Box::pin(async { Ok(String::new()) })
            }
        }

        let cfg = tmp_cfg();
        let session = tmp_session();
        let backend = Arc::new(SmallLive {
            prompt: 1_000,
            complete_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut loop_ = AgentLoop::new(
            cfg.clone(),
            backend.clone(),
            session,
            Some(Arc::new(|_, _, _| "y".to_string())),
            Some(Arc::new(|_, _| {})),
        );

        // Padding that goes OUT on turn 1's request: the live count
        // (1_000) then "covers" it, so the gate never estimates it.
        loop_.memory.push(json!({
            "role": "assistant",
            "content": "b".repeat(40_000), // ~10_000 est tokens
        }));

        // Turn 1 (small) establishes the live baseline over the whole
        // memory including the padding.
        let res = loop_.run_turn("hi").await;
        assert!(res.ok, "{}", res.reason);
        assert_eq!(res.prompt_tokens, 1_000);

        // Size turn 2's message so that:
        //   estimate(whole memory) > threshold  (old gate fires)
        //   live(1_000) + estimate(new message only) <= threshold  (new gate passes)
        let threshold = cfg.context.threshold_tokens();
        let rest = estimate_prompt_tokens(&loop_.memory);
        assert!(rest > 1_200, "precondition: memory should weigh ~10k est tokens, got {rest}");
        let tail = threshold - rest + 100; // est of just the new message
        let big = "a".repeat((tail as usize) * 4);
        assert!(
            rest + tail > threshold && 1_000 + tail <= threshold,
            "test sizing: rest={rest} tail={tail} threshold={threshold}"
        );

        let res = loop_.run_turn(&big).await;
        assert!(res.ok, "{}", res.reason);
        assert_eq!(res.prompt_tokens, 1_000);
        // No summarizer call: the estimate overcount did not trigger compaction.
        assert_eq!(backend.complete_calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    // ---------- reasoning_content: echoed back to the server ----------

    /// Backend that streams a thinking block before the text.
    struct ReasoningStream {
        n: u8,
    }
    impl futures_util::Stream for ReasoningStream {
        type Item = Result<StreamItem, BackendError>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            use crate::backend::SseEvent;
            match self.n {
                0 => {
                    self.n = 1;
                    std::task::Poll::Ready(Some(Ok(StreamItem::Event(
                        SseEvent::Reasoning("let me think".into()),
                    ))))
                }
                1 => {
                    self.n = 2;
                    std::task::Poll::Ready(Some(Ok(StreamItem::Event(SseEvent::Text(
                        "answer".into(),
                    )))))
                }
                _ => std::task::Poll::Ready(Some(Ok(StreamItem::Done {
                    finish_reason: "stop".into(),
                    usage: None,
                }))),
            }
        }
    }

    struct ReasoningBackend;
    impl Backend for ReasoningBackend {
        fn stream_chat<'a>(&'a self, _m: &'a [Value], _t: Option<&'a [Value]>) -> ChatStream<'a> {
            Box::pin(async {
                Ok(Box::pin(ReasoningStream { n: 0 }) as _)
            })
        }
        fn complete<'a>(&'a self, _m: &'a [Value], _t: u64) -> CompleteFut<'a> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    #[tokio::test]
    async fn reasoning_is_echoed_to_memory_jsonl_and_replay() {
        let cfg = tmp_cfg();
        let session = tmp_session();
        let mut loop_ = AgentLoop::new(
            cfg,
            Arc::new(ReasoningBackend),
            session,
            Some(Arc::new(|_, _, _| "y".to_string())),
            Some(Arc::new(|_, _| {})),
        );
        let res = loop_.run_turn("hi").await;
        assert!(res.ok, "{}", res.reason);

        // 1. wire message in memory carries reasoning_content (so the next
        //    request to the llama.cpp server replays the thinking block)
        let last = loop_.memory.last().unwrap().clone();
        assert_eq!(last["role"], "assistant");
        assert_eq!(last["content"], "answer");
        assert_eq!(last["reasoning_content"], "let me think");

        // 2. JSONL audit log persisted it
        let ev = loop_
            .session
            .events()
            .iter()
            .rev()
            .find(|e| e.get("type").and_then(Value::as_str) == Some("msg")
                && e.get("role").and_then(Value::as_str) == Some("assistant"))
            .unwrap();
        assert_eq!(ev["reasoning_content"], "let me think");

        // 3. resume replays it into working memory
        let path = loop_.session.path.clone();
        let reopened = Session::open(&path);
        let mem = reopened.replay("SYS");
        let last = mem.last().unwrap();
        assert_eq!(last["reasoning_content"], "let me think");

        // 4. the NEXT request actually sends it (capture the messages)
        let got: Arc<std::sync::Mutex<Option<Value>>> =
            Arc::new(std::sync::Mutex::new(None));
        struct CaptureBackend {
            got: Arc<std::sync::Mutex<Option<Value>>>,
        }
        impl Backend for CaptureBackend {
            fn stream_chat<'a>(
                &'a self,
                m: &'a [Value],
                _t: Option<&'a [Value]>,
            ) -> ChatStream<'a> {
                *self.got.lock().unwrap() = Some(json!(m));
                Box::pin(async {
                    Ok(Box::pin(futures_util::stream::once(async {
                        Ok::<_, BackendError>(StreamItem::Done {
                            finish_reason: "stop".into(),
                            usage: None,
                        })
                    })) as _)
                })
            }
            fn complete<'a>(&'a self, _m: &'a [Value], _t: u64) -> CompleteFut<'a> {
                Box::pin(async { Ok(String::new()) })
            }
        }
        let mut loop2 = AgentLoop::new(
            loop_.cfg.clone(),
            Arc::new(CaptureBackend { got: got.clone() }),
            Session::open(&path),
            None,
            None,
        );
        let res2 = loop2.run_turn("again").await;
        assert!(res2.ok, "{}", res2.reason);
        let sent = got.lock().unwrap().as_ref().unwrap().clone();
        let sent: Vec<Value> =
            serde_json::from_value(sent.clone()).expect("messages array");
        let with_reasoning: Vec<&Value> = sent
            .iter()
            .filter(|m| m["role"] == "assistant")
            .filter(|m| m["reasoning_content"] == "let me think")
            .collect();
        assert_eq!(with_reasoning.len(), 1, "{:?}", sent);
    }

    #[test]
    fn record_tool_emits_vision_content_to_memory_only() {
        use crate::tools::registry::ToolResult;

        let cfg = tmp_cfg();
        let session = tmp_session();
        let mut loop_ = AgentLoop::new(
            cfg,
            Arc::new(NoopBackend),
            session,
            Some(Arc::new(|_, _, _| "y".to_string())),
            Some(Arc::new(|_, _| {})),
        );
        let result = ToolResult {
            result: "an image".into(),
            ok: true,
            images: vec!["data:image/png;base64,AAAA".into()],
        };
        loop_.record_tool("call_9", "read_file", &json!({}), result);
        // memory carries the parts array (wire format for the model)
        let last = loop_.memory.last().unwrap().clone();
        assert_eq!(last["role"], "tool");
        assert_eq!(last["tool_call_id"], "call_9");
        let parts = last["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
        // the JSONL audit log keeps the plain text (images are not
        // persisted — a resume re-reads the file when needed)
        let tool_ev = loop_
            .session
            .events()
            .iter()
            .rev()
            .find(|e| e.get("type").and_then(Value::as_str) == Some("tool"))
            .unwrap();
        assert_eq!(tool_ev["result"], "an image");
        // and a NON-vision tool result stays a plain string in memory
        loop_.record_tool(
            "call_10",
            "list_dir",
            &json!({}),
            crate::tools::registry::ToolResult::res("plain"),
        );
        let last = loop_.memory.last().unwrap();
        assert_eq!(last["content"], "plain");
    }
}
