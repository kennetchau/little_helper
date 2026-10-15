//! TUI (phase 6, plan §5-Q3/§6): ratatui + crossterm port of
//! `reference/harness/src/harness/tui/app.py`.
//!
//! Same architecture as the reference: the agent layer is headless and this
//! is a thin renderer plugged in through the `AgentLoop` constructor seams —
//! `emit` posts UI events over an mpsc channel, `ask_approval` posts the
//! request and blocks its worker on an oneshot the modal resolves (the Rust
//! analogue of Python's worker thread waiting on a `threading.Event`). The
//! loop is shared as `Arc<tokio::sync::Mutex<AgentLoop>>`; `run_turn` /
//! `compact_now` run in their own tokio tasks (mirrors Python's worker
//! threads) while the UI task stays responsive. Cancellation goes through a
//! cloned watch sender, so `cancel()` works while a turn task holds the
//! loop lock (Python's `Event.set()` is lock-free for the same reason).
//!
//! Ctrl-C / q during a turn calls `AgentLoop::cancel()` — cooperative,
//! exactly like the reference — and when idle it only offers `/quit`
//! (CR_01/6: Ctrl-C never quits the app; it stops the turn). Esc on a
//! modal denies (never hang the turn).
//!
//! Layout (CR_01/1): a pinned header row (session · model · tools) that
//! never scrolls, the conversation (stick-to-bottom, PageUp/Down,
//! Shift+↑/↓, mouse wheel), a status bar with LIVE cumulative prompt
//! tokens (CR_01/5), and an input area whose first row shows a blinking
//! block cursor (CR_01/3). The buffer is multi-line: each hard newline
//! (Shift+Enter / Ctrl-J) becomes its own visible row, the block grows
//! with them (capped at MAX_INPUT_LINES, scrolled to keep the caret's
//! row in view), and long text on one row is clamped with an ellipsis.
//! Commands:
//! `/help /models /model <name> /load <name> /unload [name] /new
//! /compact /prompt <text> /quit`.

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    Event, EventStream, KeyCode, KeyEventKind, KeyEventState, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor::Show, event::DisableMouseCapture, event::EnableMouseCapture, ExecutableCommand};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::{json, Value};

use crate::agent::loop_::{AgentLoop, DEFAULT_AGENT};
use crate::agent::session::Session;
use crate::backend::{BackendClient, ModelInfo, ModelStatus};
use crate::config::Config;
use crate::tools::registry;

const PREVIEW_CAP: usize = 4000;
const RESULT_CAP: usize = 600;
/// Lines kept for rendering; the JSONL audit log stays unbounded.
const DOC_CAP: usize = 20_000;
/// UI tick: status-bar elapsed timer + modal polling + cursor blink.
const TICK: Duration = Duration::from_millis(100);

/// Kitty keyboard protocol flags, enabled at TUI start ONLY when the
/// startup probe (`kitty_probe`) proves the terminal speaks the protocol
/// (disambiguate | report-alternate-keys | report-all-keys = 0b1101).
/// The third one is the key to Shift+Enter: the kitty spec deliberately
/// keeps sending a BARE `\r` for Enter (and a bare `\x03` for Ctrl-C)
/// even when disambiguate is on, so the shell can still run `reset` after
/// a crash. That is exactly why the earlier "fix" (which pushed only
/// disambiguate, flag 1) never worked — Shift+Enter still arrived as a
/// bare `\r`, byte-for-byte identical to plain Enter. With report-all-keys
/// on, EVERY key arrives as a `CSI u` escape, so Shift+Enter =
/// `CSI 13;2u` (Enter + SHIFT) and plain Enter = `CSI 13u` — finally
/// distinguishable (crossterm 0.28 decodes all of these in
/// `parse_csi_u_encoded_key_code`; Ctrl-C still decodes to Ctrl+Char('c')).
///
/// Probing first matters: most terminals do NOT speak kitty (tmux without
/// extended keys, stock VTE, xterm, ...). Enabling the flags there was
/// useless at best (the terminal ignores the sequence and keeps sending
/// legacy bytes — Shift+Enter still a bare `\r`) and a console beep at
/// worst. With the probe, non-kitty terminals keep the legacy behavior
/// (the universal Ctrl-J fallback is the only newline key there) and the
/// hint line tells the user which key works.
///
/// The kitty "detect before enabling" handshake (kitty docs, "Query the
/// terminal"): enable disambiguate (`CSI ?1u`), query the current flags
/// (`CSI ?15n`), wait a short time for the `CSI ?1;<flags>u` reply. It
/// runs BEFORE crossterm's EventStream starts reading stdin, so nothing
/// else races for the reply (the old "competing reader" objection applied
/// to probing mid-loop, not at startup). At worst the user's first
/// keystroke inside the ~100 ms window is eaten by the probe.
///
/// `PopKeyboardEnhancementFlags` (`CSI <1u`, restore the previous
/// keyboard mode) is executed on EVERY exit path (normal + panic) before
/// leaving the alternate screen.
fn kbd_flags() -> KeyboardEnhancementFlags {
    // a fn, not a const, because bitflags' `|` is not const-evaluable.
    //  0b1 disambiguate           — Esc/Alt/Ctrl arrive as CSI u
    //  0b4 report alternate keys  — shifted keys carry the shifted
    //                                codepoint, so a typed `A`/`!`
    //                                decodes to the real character
    //  0b8 report all keys        — THE one that matters for Shift+Enter:
    //                                it overrides the spec's "Enter stays
    //                                a bare \r" carve-out, so Shift+Enter
    //                                = CSI 13;2u, plain Enter = CSI 13u.
    // 0b2 (report event types) is deliberately OMITTED: it would make key
    // autorepeats arrive as non-Press events, which the key loop ignores —
    // that would break holding a key to repeat a character in the input.
    KeyboardEnhancementFlags::from_bits_truncate(0b1101)
}

const HELP: &str = "\
/models — list backend models with status (loaded / sleeping / unloaded)
/model <name> — switch (persists)
/load <name> — load a model into the backend
/unload [name] — unload a model (default: the current one)
in the model modal: ↑↓ move · enter switch · l load · u unload · r refresh
/new — new session
/compact — compact now
/prompt <text> — set the system prompt
/quit — exit
Ctrl-C — stop the running turn only (never quits the app)
PageUp / PageDown / Shift+↑↓ / mouse wheel — scroll the conversation
Shift+Enter (or Ctrl-J on terminals without kitty protocol support) — newline";

/// The loop's ask/emit seams (Rust analogue of Python's
/// `ask_approval` / `emit` callables). The TUI feeds both.
pub(crate) fn make_seams(
    ui_tx: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
) -> (
    Arc<dyn Fn(&str, &Value, &str) -> String + Send + Sync>,
    Arc<dyn Fn(&str, &Value) + Send + Sync>,
) {
    let ask = {
        let ui_tx = ui_tx.clone();
        Arc::new(move |name: &str, _args: &Value, preview: &str| {
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            // The UI task picks the request up on its next poll and shows
            // the modal; if the app is gone the send fails and we deny.
            if ui_tx
                .send(UiEvent::ApprovalRequest {
                    tx,
                    name: name.to_string(),
                    preview: preview.chars().take(PREVIEW_CAP).collect(),
                })
                .is_err()
            {
                return "n".to_string();
            }
            // Block this worker until the modal answers (multi-threaded
            // runtime; the UI task keeps running on the other workers —
            // same as Python's worker thread blocked on the Event).
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    match rx.await {
                        Ok(d) if d == "y" || d == "a" || d == "n" => d,
                        _ => "n".to_string(),
                    }
                })
            })
        }) as Arc<dyn Fn(&str, &Value, &str) -> String + Send + Sync>
    };
    let emit = {
        let ui_tx = ui_tx.clone();
        Arc::new(move |kind: &str, data: &Value| {
            let _ = ui_tx.send(loop_emit_event(kind, data));
        }) as Arc<dyn Fn(&str, &Value) + Send + Sync>
    };
    (ask, emit)
}

/// Translate a loop `emit(kind, data)` into a UI event.
fn loop_emit_event(kind: &str, data: &Value) -> UiEvent {
    match kind {
        "text" => UiEvent::Text(data.get("text").and_then(Value::as_str).unwrap_or("").to_string()),
        "reasoning" => UiEvent::Reasoning(
            data.get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        "tool" => UiEvent::Tool {
            name: data
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            args: data.get("args").cloned().unwrap_or(Value::Null),
        },
        "tool_result" => UiEvent::ToolResult {
            ok: data.get("ok").and_then(Value::as_bool) == Some(true),
            result: data.get("result").and_then(Value::as_str).unwrap_or("").to_string(),
        },
        "error" => UiEvent::Error(
            data.get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        "usage" => UiEvent::Usage {
            prompt: data.get("prompt").and_then(Value::as_u64).unwrap_or(0),
            completion: data.get("completion").and_then(Value::as_u64).unwrap_or(0),
        },
        _ => UiEvent::Info(data.get("text").and_then(Value::as_str).unwrap_or("").to_string()),
    }
}

// ---------- UI events (agent task -> UI task; the Rust analogue of
// ---------- Textual's post_message / call_from_thread)

pub(crate) enum UiEvent {
    Text(String),
    Reasoning(String),
    Tool { name: String, args: Value },
    ToolResult { ok: bool, result: String },
    Error(String),
    Info(String),
    /// Runtime warning forwarded from `logmsg` (TUI sink).
    Warn(String),
    /// Live token counts (CR_01/5), emitted mid-stream as the backend's
    /// own counters arrive per chunk, and once more when each request
    /// finishes (backend totals, or the local estimate when the backend
    /// reports nothing).
    Usage { prompt: u64, completion: u64 },
    TurnDone(crate::agent::loop_::TurnResult),
    CompactDone(bool),
    /// Fresh /models result (the `/models` command): opens the model
    /// modal, or updates it in place when it is already open.
    Models(Vec<ModelInfo>),
    /// Live re-poll from a running load/unload operation: only updates
    /// the modal when it is open (never re-opens it).
    ModelsRefresh(Vec<ModelInfo>),
    /// A load/unload operation finished (modal note, or a conversation
    /// line when the modal was closed in the meantime). `verb` is
    /// "loaded" / "unloaded".
    ModelOpDone {
        name: String,
        verb: String,
        ok: bool,
        detail: String,
    },
    ModelsFailed(String),
    ApprovalRequest {
        tx: tokio::sync::oneshot::Sender<String>,
        name: String,
        preview: String,
    },
}

// ---------- modals (y / n / a on approval; model manager) ----------

enum Modal {
    Approval {
        name: String,
        preview: String,
        tx: tokio::sync::oneshot::Sender<String>,
    },
    /// Model manager: status per model + load / unload / switch.
    /// `sel` is the highlighted row (arrows); `note` is the last
    /// operation result. The in-flight load/unload is tracked on the
    /// App (`model_op`), not here — it keeps running even after the
    /// modal is closed.
    Model {
        models: Vec<ModelInfo>,
        sel: usize,
        note: Option<String>,
    },
}

// ---------- app ----------

/// Kind of conversation section, used for the blank-line separators
/// between sections (see `App::gap_if`). Tool results are recorded as
/// `Tool` so a call and its result stay attached as one block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Section {
    User,
    Text,
    Reasoning,
    Info,
    Tool,
    Error,
    Note,
}

struct App {
    cfg: Config,
    client: Arc<BackendClient>,
    session: Session,
    loop_: Arc<tokio::sync::Mutex<AgentLoop>>,
    /// Lock-free cancel handle (cloned from the loop's watch sender).
    cancel_tx: tokio::sync::watch::Sender<bool>,
    /// Pinned header row (CR_01/1): session · model · tools. Rendered in its
    /// own layout area so it never scrolls with the conversation.
    header: Line<'static>,
    doc: Vec<Line<'static>>,
    /// Style of the line currently being streamed — Some only while the
    /// last doc line is an open text/reasoning stream. Consecutive
    /// deltas in the same style extend that line instead of starting a
    /// new one (Python: `conv.stream()` appends without a newline); any
    /// discrete event, style change, or line break closes the stream.
    /// (Style alone can't identify the stream line — tool-result lines
    /// also end in a default-styled span.)
    stream_style: Option<Style>,
    /// Index in `doc` of the open stream line; None = no open stream.
    stream_line: Option<usize>,
    /// Logical lines scrolled off the top of `doc` (bounded document).
    base: usize,
    /// User scroll from the bottom; None = stuck to the bottom.
    user_scroll: Option<usize>,
    /// Terminal speaks the kitty keyboard protocol (startup probe) —
    /// decides which newline key the hint line advertises.
    kitty: bool,
    /// Section kind of the last conversation line rendered; None when
    /// the document is empty. Drives the blank-line separators between
    /// message / thinking / tool / error / note sections.
    last_kind: Option<Section>,
    // status bar — CUMULATIVE across the session, updated live during a
    // turn (CR_01/5). prompt = max of every in-turn backend count;
    // completion is the turn's running total as reported by the backend
    // counters streamed WITH the completion (per-chunk `timings` /
    // `usage` objects), not a local estimate.
    tokens: u64,
    completion_tokens: u64,
    commit: Option<String>,
    working: bool,
    working_what: String,
    working_since: Option<Instant>,
    modal: Option<Modal>,
    /// Model name of the in-flight backend load/unload operation (the
    /// background task keeps running even when the model modal is
    /// closed); None = no operation running.
    model_op: Option<String>,
    /// Input buffer (CR_01/3): a single string that may contain newlines
    /// (Shift+Enter inserts one). `input_pos` is the char index of the
    /// cursor within it (0..=input.chars().count()).
    input: String,
    input_pos: usize,
    quitting: bool,
}

impl App {
    fn new(
        cfg: Config,
        client: Arc<BackendClient>,
        session: Session,
        loop_: Arc<tokio::sync::Mutex<AgentLoop>>,
        cancel_tx: tokio::sync::watch::Sender<bool>,
        kitty: bool,
    ) -> Self {
        let tools = registry::enabled_names(&cfg).len();
        let header = Line::from(format!(
            " session {} · model {} · {tools} tools ",
            session.id(),
            client.model()
        ))
        .style(Style::default().fg(Color::Black).bg(Color::DarkGray));
        Self {
            cfg,
            client,
            session,
            loop_,
            cancel_tx,
            header,
            doc: Vec::new(),
            stream_style: None,
            stream_line: None,
            base: 0,
            user_scroll: None,
            kitty,
            last_kind: None,
            tokens: 0,
            completion_tokens: 0,
            commit: None,
            working: false,
            working_what: "thinking".into(),
            working_since: None,
            modal: None,
            model_op: None,
            input: String::new(),
            input_pos: 0,
            quitting: false,
        }
    }

    // -- input editing (CR_01/3): multi-line, cursor = char index ----------
    // (pub(crate) so the render/input logic is unit-testable)

    pub(crate) fn input_char_count(&self) -> usize {
        self.input.chars().count()
    }

    pub(crate) fn insert_input(&mut self, s: &str) {
        let pos = self.input_pos;
        let before: String = self.input.chars().take(pos).collect();
        let after: String = self.input.chars().skip(pos).collect();
        self.input = format!("{before}{s}{after}");
        self.input_pos = pos + s.chars().count();
    }

    pub(crate) fn backspace_input(&mut self) {
        if self.input_pos == 0 {
            return;
        }
        let pos = self.input_pos;
        let before: String = self.input.chars().take(pos - 1).collect();
        let after: String = self.input.chars().skip(pos).collect();
        self.input = format!("{before}{after}");
        self.input_pos = pos - 1;
    }

    pub(crate) fn delete_input(&mut self) {
        let pos = self.input_pos;
        if pos >= self.input_char_count() {
            return;
        }
        let before: String = self.input.chars().take(pos).collect();
        let after: String = self.input.chars().skip(pos + 1).collect();
        self.input = format!("{before}{after}");
    }

    /// Left / right movement across the whole buffer (newlines included).
    pub(crate) fn move_input(&mut self, delta: i64) {
        let pos = self.input_pos;
        let n = self.input_char_count() as i64;
        let np = (pos as i64 + delta).clamp(0, n);
        self.input_pos = np as usize;
    }

    /// Append a streamed text delta (may span multiple lines). A delta
    /// in the same style as the line currently being streamed extends
    /// that line with no newline (Python: `conv.stream()`); anything
    /// else — a different style, a line break, or a discrete event —
    /// starts a new line. Without this, every per-token delta would
    /// render on its own line.
    ///
    /// The FIRST line of a new agent-text block is prefixed with "▸ "
    /// (the block marker that separates the agent's message from the
    /// dim user/tool/other sections). A block starts after any
    /// discrete line or section change; continuation lines (after a
    /// hard break inside the same block) carry no marker.
    fn append_text(&mut self, text: &str, style: Style, kind: Section) {
        if text.is_empty() {
            return;
        }
        let mut seen = false; // a line of this delta was already opened
        for (i, chunk) in text.split_inclusive('\n').enumerate() {
            let body = chunk.strip_suffix('\n').unwrap_or(chunk);
            if i == 0 && body.is_empty() {
                continue; // empty leading chunk (leading newline)
            }
            let extend = i == 0 && self.open_stream_is(style);
            // Continuation = no "▸ " block marker: true for every line
            // after this delta's first, and for the delta's first non-empty
            // chunk when the last doc line is still this same stream's line
            // (no discrete event or section gap since the block started).
            let cont = if seen {
                true
            } else {
                self.last_line_is_stream(style)
            };
            if extend {
                // extend the open stream line in place (span content is a
                // Cow, so rebuild it owned rather than pushing)
                let line = self.doc.last_mut().unwrap();
                let span = line.spans.last_mut().unwrap();
                let mut joined =
                    String::with_capacity(span.content.len() + body.len());
                joined.push_str(&span.content);
                joined.push_str(body);
                *span = Span::styled(joined, style);
                seen = true;
            } else if !body.is_empty() {
                let body = if cont {
                    body.to_string()
                } else if kind == Section::Text {
                    format!("▸ {body}")
                } else {
                    body.to_string()
                };
                self.open_stream_line(&body, style, kind);
                seen = true;
            }
        }
        if !text.ends_with('\n') {
            self.stream_style = Some(style);
        } else {
            self.stream_style = None; // trailing line break closed it
        }
    }

    /// Is the last doc line an open stream in `style`? The recorded
    /// index proves the streamed line is still present and still last —
    /// a doc clear, a DOC_CAP drop, or any pushed line in between
    /// invalidates the match (and `push_line`/`clear_doc` also reset the
    /// state themselves).
    fn open_stream_is(&self, style: Style) -> bool {
        match (&self.stream_style, self.stream_line) {
            (Some(s), Some(idx)) if *s == style => idx == self.doc.len() - 1,
            _ => false,
        }
    }

    /// Is the last doc line a line of the stream in `style` — open or
    /// just closed by a trailing newline? Stream lines are single-span,
    /// so the span's style identifies the stream that opened the line.
    /// Any intervening discrete line, section gap, DOC_CAP drop, or doc
    /// clear invalidates the match (they reset `stream_line`).
    fn last_line_is_stream(&self, style: Style) -> bool {
        match self.stream_line {
            Some(idx) if idx == self.doc.len() - 1 => self.doc[idx]
                .spans
                .last()
                .is_some_and(|s| s.style == style),
            _ => false,
        }
    }

    /// Start (or restart) the open stream line.
    fn open_stream_line(&mut self, body: &str, style: Style, kind: Section) {
        self.push_line(Line::from(Span::styled(body.to_string(), style)), kind);
        self.stream_line = Some(self.doc.len() - 1);
        self.stream_style = Some(style);
    }

    /// One discrete line (tool call, result, info, error, turn header).
    /// It always breaks the stream — the next delta starts a fresh line
    /// (Python: discrete events call `conv.flush()` before writing).
    fn push_line(&mut self, line: Line<'static>, kind: Section) {
        self.gap_if(kind);
        self.stream_style = None;
        self.stream_line = None;
        self.doc.push(line);
        if self.doc.len() > DOC_CAP {
            // keep the renderable document bounded (the JSONL audit log is
            // not) — dropped lines count against `base` for scroll math
            let mut excess = self.doc.len() - DOC_CAP;
            while excess > 0 {
                let dropped = line_width(&self.doc[0]).max(1);
                let drop = dropped.min(excess);
                excess -= dropped;
                self.doc.remove(0);
                self.base = self.base.saturating_add(drop);
            }
        }
        self.user_scroll = None; // stick to the bottom on new content
    }

    /// Blank separator line between two adjacent conversation lines of
    /// DIFFERENT sections, so message / thinking / tool / error / note
    /// blocks read apart. No separator within a section (a tool call and
    /// its result; new paragraphs of one stream — continuations extend
    /// the open stream line and never reach here).
    fn gap_if(&mut self, kind: Section) {
        if self.last_kind == Some(kind) {
            return;
        }
        let last_empty = self.doc.last().map_or(true, |l| line_width(l) == 0);
        if !last_empty {
            // the separator also breaks any open stream (the next delta
            // must not extend into the section above it)
            self.stream_style = None;
            self.stream_line = None;
            self.doc.push(Line::default());
        }
        self.last_kind = Some(kind);
    }

    fn clear_doc(&mut self) {
        self.doc.clear();
        self.stream_style = None;
        self.stream_line = None;
        self.base = 0;
        self.user_scroll = None;
        self.last_kind = None;
    }

    // -- emit seam (consumes the loop's emit events) -------------------------

    fn on_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Text(t) => {
                self.set_working(true, "answering");
                self.append_text(&t, Style::default(), Section::Text);
            }
            UiEvent::Reasoning(t) => {
                self.set_working(true, "thinking");
                self.append_text(
                    &t,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                    Section::Reasoning,
                );
            }
            UiEvent::Tool { name, args } => {
                // CR_01/4: the label shows exactly what the model is doing.
                self.set_working(true, &format!("tool: {name}"));
                let shown = if args.is_object() {
                    args
                } else {
                    json!({"raw": args})
                };
                self.push_line(
                    Line::from(format!("⚙ {name} {}", shown))
                        .style(Style::default().fg(Color::Cyan)),
                    Section::Tool,
                );
            }
            UiEvent::ToolResult { ok, result } => {
                self.set_working(true, "waiting for model");
                let mark = if ok { "✓" } else { "✗" };
                let mark_style = if ok {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Red)
                };
                // CR_01/9: display-only pretty-print — when the result is
                // JSON, show it formatted (the model still gets the raw
                // text; the JSONL log is untouched).
                let pretty = pretty_result(&result);
                let mut s = String::new();
                for (i, ch) in pretty.chars().enumerate() {
                    if i >= RESULT_CAP {
                        s.push('…');
                        break;
                    }
                    s.push(ch);
                }
                // The result body is DIM so the agent's message (default
                // color, "▸ "-marked) reads as the primary content.
                self.push_line(Line::from(vec![
                    Span::styled(format!("  {mark} "), mark_style),
                    Span::styled(s, Style::default().fg(Color::DarkGray)),
                ]), Section::Tool);
            }
            UiEvent::Error(t) => {
                self.push_line(
                    Line::from(format!("error: {t}")).style(
                        Style::default()
                            .fg(Color::Red)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Section::Error,
                );
            }
            UiEvent::Usage { prompt, completion } => {
                // Live per-request counts (CR_01/5): prompt is cumulative
                // (max — the context only grows within a session); the loop
                // sends its running completion total for the current turn,
                // so it's set (each Usage supersedes the last).
                self.tokens = self.tokens.max(prompt);
                self.completion_tokens = completion;
            }
            UiEvent::Info(t) => {
                self.append_text(&t, Style::default().fg(Color::DarkGray), Section::Info);
            }
            UiEvent::TurnDone(res) => {
                self.set_working(false, "thinking");
                self.tokens = self.tokens.max(res.prompt_tokens);
                if res.commit.is_some() {
                    self.commit = res.commit.clone();
                }
                if res.ok {
                    let mut bits = vec![format!(
                        "turn done · prompt_tokens={} · completion_tokens={}",
                        res.prompt_tokens, res.completion_tokens
                    )];
                    if let Some(c) = &res.commit {
                        bits.push(format!("workspace@{c}"));
                    }
                    self.push_line(
                        Line::from(bits.join(" · ")).style(Style::default().fg(Color::DarkGray)),
                        Section::Note,
                    );
                } else {
                    self.push_line(
                        Line::from(format!("turn failed: {}", res.reason)).style(
                            Style::default()
                                .fg(Color::Red)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Section::Error,
                    );
                }
            }
            UiEvent::CompactDone(true) => {
                self.set_working(false, "thinking");
                self.push_line(
                    Line::from("context compacted").style(Style::default().fg(Color::DarkGray)),
                    Section::Note,
                );
            }
            UiEvent::CompactDone(false) => {
                self.set_working(false, "thinking");
            }
            UiEvent::Models(models) => {
                // A re-query while the modal is already open updates it
                // in place (keeping selection + last note); a fresh
                // query from the `/models` command opens it — the
                // selection starts on the current model.
                if let Some(Modal::Model { sel, note, .. }) = self.modal.take() {
                    self.modal = Some(Modal::Model {
                        sel: sel.min(models.len().saturating_sub(1)),
                        models,
                        note,
                    });
                    return;
                }
                if models.is_empty() {
                    self.push_line(
                        Line::from("backend returned no models").style(
                            Style::default()
                                .fg(Color::Red)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Section::Error,
                    );
                    return;
                }
                let sel = models
                    .iter()
                    .position(|m| m.id == self.client.model())
                    .unwrap_or(0);
                self.modal = Some(Modal::Model {
                    models,
                    sel,
                    note: None,
                });
            }
            UiEvent::ModelsRefresh(models) => {
                // live re-poll from a running op: only when the modal is
                // open (the conversation gets the result line via
                // ModelOpDone instead)
                if let Some(Modal::Model { sel, note, .. }) = self.modal.take() {
                    self.modal = Some(Modal::Model {
                        sel: sel.min(models.len().saturating_sub(1)),
                        models,
                        note,
                    });
                }
            }
            UiEvent::ModelOpDone {
                name,
                verb,
                ok,
                detail,
            } => {
                let note = if ok {
                    format!("{verb} {name}")
                } else {
                    format!("{verb} {name} failed: {detail}")
                };
                let style = if ok {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Red)
                };
                // the op marker is app-level (survives closing the modal)
                if self.model_op.as_deref() == Some(name.as_str()) {
                    self.model_op = None;
                }
                let Some(Modal::Model { models, sel, .. }) = self.modal.take() else {
                    // modal was closed meanwhile: land in the conversation
                    self.push_line(Line::from(note).style(style), Section::Info);
                    return;
                };
                // modal still open: the result becomes its note.
                // Selection and list are kept.
                self.modal = Some(Modal::Model {
                    models,
                    sel,
                    note: Some(note),
                });
            }
            UiEvent::ModelsFailed(msg) => {
                self.push_line(
                    Line::from(format!("model list failed: {msg}")).style(
                        Style::default()
                            .fg(Color::Red)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Section::Error,
                );
            }
            UiEvent::Warn(msg) => {
                // Runtime warnings forwarded through logmsg (git, session,
                // backend, config). Rendered in the conversation instead of
                // spilling raw stderr over the alternate screen — that raw
                // leak is what produced the "weird text" garbage over the UI.
                for (i, l) in msg.lines().enumerate() {
                    let text = if i == 0 { format!("⚠ {l}") } else { format!("  │ {l}") };
                    self.push_line(
                        Line::from(text).style(Style::default().fg(Color::Red)),
                        Section::Error,
                    );
                }
            }
            UiEvent::ApprovalRequest { tx, name, preview } => {
                // the turn task is blocked on the oneshot until the modal
                // resolves — same as the reference worker thread
                self.modal = Some(Modal::Approval {
                    name,
                    preview,
                    tx,
                });
            }
        }
    }

    fn set_working(&mut self, active: bool, what: &str) {
        if active && !self.working {
            self.working_since = Some(Instant::now());
        }
        if !active {
            self.working_since = None;
        }
        self.working = active;
        self.working_what = what.to_string();
    }

    /// Rebuild the pinned header (session · model · tools).
    fn refresh_header(&mut self) {
        self.header = Line::from(format!(
            " session {} · model {} · {} tools ",
            self.session.id(),
            self.client.model(),
            registry::enabled_names(&self.cfg).len()
        ))
        .style(Style::default().fg(Color::Black).bg(Color::DarkGray));
    }

    // -- commands -------------------------------------------------------------

    async fn dispatch(
        &mut self,
        cmd: &str,
        arg: &str,
        ui_tx: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
    ) {
        match cmd {
            "help" => self.on_event(UiEvent::Info(HELP.to_string())),
            "new" => {
                if self.working {
                    self.on_event(UiEvent::Info(
                        "a turn is running — finish it first".into(),
                    ));
                    return;
                }
                let agent = self
                    .loop_
                    .lock()
                    .await
                    .session
                    .agent();
                let session = Session::create(&agent, &self.client.model(), None);
                let session_for_app = Session::open(&session.path);
                let (ask, emit) = make_seams(ui_tx);
                let loop_ = AgentLoop::new(
                    self.cfg.clone(),
                    self.client.clone(),
                    session,
                    Some(ask),
                    Some(emit),
                );
                let cancel_tx = loop_.cancel_sender();
                self.loop_ = Arc::new(tokio::sync::Mutex::new(loop_));
                self.cancel_tx = cancel_tx;
                self.session = session_for_app;
                self.clear_doc();
                self.tokens = 0;
                self.completion_tokens = 0;
                self.commit = None;
                self.refresh_header();
                self.push_line(
                    Line::from(format!("new session {}", self.session.id()))
                        .style(Style::default().fg(Color::DarkGray)),
                    Section::Info,
                );
            }
            "models" => {
                self.on_event(UiEvent::Info("querying /models …".into()));
                let client = self.client.clone();
                let tx = ui_tx.clone();
                tokio::spawn(async move {
                    match client.list_models().await {
                        Ok(m) => {
                            let _ = tx.send(UiEvent::Models(m));
                        }
                        Err(e) => {
                            let _ = tx.send(UiEvent::ModelsFailed(e.to_string()));
                        }
                    }
                });
            }
            "model" => {
                if arg.is_empty() {
                    self.on_event(UiEvent::Info(format!(
                        "current model: {}",
                        self.client.model()
                    )));
                } else {
                    self.switch_model(arg);
                }
            }
            "load" => {
                if arg.is_empty() {
                    self.on_event(UiEvent::Info("usage: /load <name>".into()));
                    return;
                }
                self.push_line(
                    Line::from(format!("loading {arg} …")).style(Style::default().fg(Color::DarkGray)),
                    Section::Info,
                );
                self.model_op(arg.to_string(), true, ui_tx);
            }
            "unload" => {
                let name = if arg.is_empty() {
                    self.client.model()
                } else {
                    arg.to_string()
                };
                self.push_line(
                    Line::from(format!("unloading {name} …")).style(Style::default().fg(Color::DarkGray)),
                    Section::Info,
                );
                self.model_op(name, false, ui_tx);
            }
            "compact" => {
                if self.working {
                    self.on_event(UiEvent::Info(
                        "a turn is running — finish it first".into(),
                    ));
                    return;
                }
                self.set_working(true, "compacting");
                let loop_ = self.loop_.clone();
                let tx = ui_tx.clone();
                tokio::spawn(async move {
                    let ok = loop_.lock().await.compact_now().await;
                    let _ = tx.send(UiEvent::CompactDone(ok));
                });
            }
            "prompt" => {
                if arg.is_empty() {
                    self.on_event(UiEvent::Info(
                        "usage: /prompt <text> — sets the system prompt".into(),
                    ));
                    return;
                }
                self.loop_.lock().await.update_system_prompt(arg);
                self.on_event(UiEvent::Info(format!("system prompt updated: {arg}")));
            }
            "quit" => {
                self.quitting = true;
            }
            other => {
                self.on_event(UiEvent::Info(format!(
                    "unknown command /{other} — try /help"
                )));
            }
        }
    }

    fn switch_model(&mut self, model: &str) {
        if model == self.client.model() {
            return;
        }
        self.client.set_model(model);
        let persisted = match persist_model(&crate::config::config_path(), model) {
            Ok(()) => "saved to config.toml".to_string(),
            Err(e) => format!("persist failed: {e}"),
        };
        // session log: model event (same as the reference app)
        self.session
            .append(json!({ "type": "model", "name": model }));
        self.on_event(UiEvent::Info(format!("model → {model} · {persisted}")));
    }

    /// Backend model operation (load / unload) via the llama.cpp router's
    /// management API (`POST /models/load` | `POST /models/unload`).
    /// It is asynchronous: after the POST, the task re-polls `/models`
    /// once a second (live-updating the open modal via ModelsRefresh)
    /// until the target model's state settles — loading a big model can
    /// take minutes — with a hard 120 s cap. The modal may be closed in
    /// the meantime; the final result lands in the conversation via
    /// ModelOpDone either way.
    fn model_op(
        &mut self,
        name: String,
        load: bool,
        ui_tx: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
    ) {
        if self.model_op.is_some() {
            self.on_event(UiEvent::Info(
                "a model operation is already in progress — wait for it to finish".into(),
            ));
            return;
        }
        self.model_op = Some(name.clone());
        let client = self.client.clone();
        let tx = ui_tx.clone();
        tokio::spawn(async move {
            let call = if load {
                client.load_model(&name).await
            } else {
                client.unload_model(&name).await
            };
            let mut ok = matches!(call, Ok(()));
            let mut detail = call.err().map(|e| e.to_string()).unwrap_or_default();
            if ok {
                // wait for the state to settle (1s polls, 120 s cap)
                let deadline = Instant::now() + Duration::from_secs(120);
                while Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    match client.list_models().await {
                        Ok(models) => {
                            let _ = tx.send(UiEvent::ModelsRefresh(models.clone()));
                            // target absent (source removed) counts as settled
                            let settled = models
                                .iter()
                                .find(|m| m.id == name)
                                .map_or(true, |m| !m.is_transitional());
                            if settled {
                                break;
                            }
                        }
                        Err(e) => {
                            ok = false;
                            detail = e.to_string();
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(UiEvent::ModelOpDone {
                name,
                verb: (if ok {
                    if load { "loaded" } else { "unloaded" }
                } else if load {
                    "loading"
                } else {
                    "unloading"
                })
                .to_string(),
                ok,
                detail,
            });
        });
    }

    async fn start_turn(
        &mut self,
        prompt: &str,
        ui_tx: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
    ) {
        // The turn number comes from the LOOP's session (the one that
        // records the turn_end events) — the app's own Session view is
        // a stale snapshot opened at startup and would always say 1.
        let turn_n = self.loop_.lock().await.session.turn_number() + 1;
        self.push_line(
            Line::from(format!("─ turn {turn_n} {}", "─".repeat(24)))
                .style(
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ),
            Section::Note,
        );
        // Show the user's request (distinguishable: green, "❯ "-marked,
        // one line per hard line of the prompt). The loop still records
        // it to the JSONL; this is display only.
        for line in user_request_lines(prompt) {
            self.push_line(line, Section::User);
        }
        self.set_working(true, "thinking");
        let loop_ = self.loop_.clone();
        let tx = ui_tx.clone();
        let prompt = prompt.to_string();
        tokio::spawn(async move {
            // the guard is a tokio guard: it is Send, so holding it across
            // the await is fine; cancel() doesn't need the lock (watch)
            let res = {
                let mut loop_ = loop_.lock().await;
                loop_.run_turn(&prompt).await
            };
            let _ = tx.send(UiEvent::TurnDone(res));
        });
    }

    /// Resolve the approval modal (None denies — Esc / quit / modal closed).
    fn answer_approval(&mut self, decision: Option<&str>) {
        let Some(Modal::Approval { tx, .. }) = self.modal.take() else {
            return;
        };
        let d = match decision {
            Some(d) if d == "y" || d == "a" || d == "n" => d.to_string(),
            _ => "n".to_string(),
        };
        let _ = tx.send(d);
    }

    /// Stop the running turn (Ctrl-C). Cooperative: the loop stops at the
    /// next safe point. Never quits the app (CR_01/6).
    fn stop_turn(&mut self) {
        let _ = self.cancel_tx.send(true);
        self.push_line(
            Line::from("stop requested (takes effect at the next safe point)")
                .style(Style::default().fg(Color::DarkGray)),
            Section::Info,
        );
    }

    /// Scroll the conversation up by `n` logical (wrapped) lines,
    /// clamped at the top of the document.
    fn scroll_up(&mut self, n: usize) {
        let (w, _) = term_size();
        let total = self
            .base
            + wrapped_counts(&self.doc, (w as usize).max(1))
                .iter()
                .sum::<usize>();
        let max = total
            .saturating_sub(conv_visible_height(input_block_lines(
                &self.input,
                w as usize,
            )));
        let cur = self.user_scroll.unwrap_or(0).saturating_add(n);
        self.user_scroll = Some(cur.min(max));
    }

    /// Scroll down by `n` logical lines; 0 re-sticks to the bottom.
    fn scroll_down(&mut self, n: usize) {
        let cur = self.user_scroll.unwrap_or(0).saturating_sub(n);
        self.user_scroll = if cur == 0 { None } else { Some(cur) };
    }
}

/// Set `[backend] model` in config.toml (line replace, or append) — port of
/// the reference `_persist_model`.
fn persist_model(path: &std::path::Path, model: &str) -> std::io::Result<()> {
    let re = regex::Regex::new(r"(?m)^model\s*=").unwrap();
    if let Ok(text) = std::fs::read_to_string(path) {
        let new = if re.is_match(&text) {
            re.replace(&text, format!("model = \"{model}\"").as_str()).to_string()
        } else {
            format!("{}[backend]\nmodel = \"{model}\"\n", text.trim_end())
        };
        std::fs::write(path, new)?;
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("[backend]\nmodel = \"{model}\"\n"))?;
    Ok(())
}

// ---------- rendering helpers -------------------------------------------------

/// CR_01/9: display-only pretty-print for tool results. When the result
/// parses as a JSON object/array, show it formatted; otherwise return it
/// unchanged. Display only — the model and the JSONL log keep the raw
/// text (it is already plain-text by design, which is what the model wants).
pub(crate) fn pretty_result(result: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(result) {
        if v.is_object() || v.is_array() {
            if let Ok(p) = serde_json::to_string_pretty(&v) {
                if p.len() <= result.len() + 200 {
                    return p;
                }
            }
        }
    }
    result.to_string()
}

/// Display-width estimate (ASCII = 1, other = 2): good enough for the
/// modal box sizing and wrap-line math; ratatui reflows the real text.
fn est_width(s: &str) -> usize {
    s.chars()
        .map(|c| if c.is_ascii() { 1 } else { 2 })
        .sum()
}

fn line_width(line: &Line) -> usize {
    line.spans.iter().map(|s| est_width(s.content.as_ref())).sum()
}

/// Wrapped height of each doc line at the given width (character-based
/// approximation; ratatui wraps on words, the drift is a few lines at most).
fn wrapped_counts(lines: &[Line], width: usize) -> Vec<usize> {
    if width == 0 {
        return vec![1; lines.len()];
    }
    lines
        .iter()
        .map(|l| line_width(l).div_ceil(width).max(1))
        .collect()
}

fn term_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

/// Rendered height of the conversation area: the full terminal height
/// minus the pinned header, the status bar and the input block
/// (1 + 1 + input rows + 2, see `draw_frame`'s layout).
fn conv_visible_height(input_rows: usize) -> usize {
    let (_, h) = term_size();
    (h as usize).saturating_sub(1 + 1 + 2 + input_rows).max(1)
}

/// Top offset (logical lines hidden above) at which to draw the
/// conversation. `user_scroll` is the user's distance FROM THE BOTTOM of
/// the document (None = stuck to the bottom), so scrolling up decreases
/// the offset — the old code used it AS the offset, which is why PageUp
/// jumped near the top of the log instead of scrolling a page up.
pub(crate) fn scroll_offset(user_scroll: Option<usize>, total: usize, visible: usize) -> usize {
    let bottom = total.saturating_sub(visible);
    match user_scroll {
        Some(s) => bottom.saturating_sub(s),
        None => bottom,
    }
}

/// Decode a key's `KeyCode::Char` payload into the character to insert
/// into the input box (CR_01/3 multi-line input). With the kitty "report
/// alternate keys" flag the terminal sends the shifted codepoint and
/// crossterm decodes it to the real character (typed `A` -> `Char('A')`,
/// `!` -> `Char('!')`); without kitty support the terminal sends the raw
/// shifted byte, which crossterm already decodes the same way. Non-ASCII
/// (e.g. `é`) decodes to its own codepoint either way. The one gap: the
/// spec sends the shifted alternate key only for the SHIFT MODIFIER —
/// Caps Lock is a lock STATE, not a modifier, so `a` with Caps on arrives
/// as `CSI 97;65u` = Char('a') + CAPS_LOCK. Re-apply the shift so Caps
/// Lock keeps working as the user expects.
fn input_char(c: char, modifiers: KeyModifiers, state: KeyEventState) -> char {
    if state.contains(KeyEventState::CAPS_LOCK) && modifiers == KeyModifiers::empty() {
        c.to_ascii_uppercase()
    } else {
        c
    }
}

// ---------- kitty protocol detection -------------------------------------------

/// Find `needle` in `hay` (simple linear scan; buffers are tiny).
fn find_subseq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Parse the reply to the kitty `CSI ?15n` query: `CSI ? 1 ; <flags> u`.
/// Returns the enabled flag bits, or None for anything else (junk, a
/// partial sequence, a primary-device-attributes reply, or a real key
/// the user typed during the probe window).
pub(crate) fn parse_kitty_probe_response(buf: &[u8]) -> Option<u8> {
    const MARK: &[u8] = b"\x1b[?1;";
    let pos = find_subseq(buf, MARK)?;
    let rest = &buf[pos + MARK.len()..];
    let end = rest.iter().position(|&b| b == b'u')?;
    let digits = &rest[..end];
    if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut flags = 0u8;
    for &b in digits {
        flags = flags.saturating_mul(10).saturating_add(b - b'0');
    }
    Some(flags)
}

/// Kitty "detect before enabling" handshake (kitty docs, "Query the
/// terminal"): enable disambiguate (`CSI ?1u` — harmless on legacy
/// terminals: Esc/Ctrl/Alt then arrive as `CSI u`, which crossterm
/// decodes to the same keys), then query the current keyboard flags
/// (`CSI ?15n`) and wait for the `CSI ?1;<flags>u` reply.
///
/// Returns true only when the terminal actually answered — i.e. it
/// speaks the protocol and Shift+Enter (`CSI 13;2u`) is distinguishable
/// from Enter (`CSI 13u`). MUST be called before crossterm's
/// `EventStream` starts reading stdin, so the reply cannot be eaten by
/// (or interleaved with) the main event loop.
///
/// Deliberately SYNCHRONOUS (nix `poll` + `read` on stdin against a
/// 100 ms deadline). The old version ran this on a throwaway tokio
/// runtime and dropped it when `prepare_terminal` returned — but after
/// `tokio::io::stdin()` has registered the tty, dropping the runtime
/// blocks indefinitely on this platform (the IO driver's shutdown only
/// completes once the tty delivers another event). Startup then sat on
/// the black alternate screen until the user's first keystroke, which
/// is what shipped as "screen is black until you click a button". No
/// runtime here — nothing to drop, nothing to hang.
fn kitty_probe() -> bool {
    use nix::poll::{poll, PollFd, PollFlags};
    use nix::unistd::read as nix_read;
    use std::os::fd::AsFd;

    let mut out = std::io::stdout();
    if out.write_all(b"\x1b[?1u\x1b[?15n").is_err() {
        return false;
    }
    let _ = out.flush();
    let mut acc: Vec<u8> = Vec::new();
    // 100 ms: a supporting terminal answers within a few ms locally (10-50
    // ms over ssh), so this rarely costs startup time; and the read window
    // is the only thing that can swallow a very early keystroke, so it
    // must be short.
    let deadline = Instant::now() + Duration::from_millis(100);
    let stdin = std::io::stdin();
    let stdin_fd = stdin.as_fd();
    while deadline > Instant::now() {
        // nix 0.30 takes the poll timeout as a plain millisecond count
        // (u16/u8 From impls); the 100 ms window never overflows
        let remaining_ms = (deadline - Instant::now()).as_millis().min(u16::MAX as u128) as u16;
        let mut fds = [PollFd::new(stdin_fd, PollFlags::POLLIN)];
        match poll(&mut fds, remaining_ms) {
            Ok(0) => break, // timeout: no reply
            Ok(_) => {
                let mut buf = [0u8; 64];
                let n = match nix_read(stdin_fd, &mut buf) {
                    Ok(n) if n > 0 => n,
                    _ => break, // EOF or read error: no reply
                };
                acc.extend_from_slice(&buf[..n]);
                if parse_kitty_probe_response(&acc).is_some() {
                    return true;
                }
                if acc.len() > 512 {
                    break; // runaway data: not a protocol reply
                }
            }
            Err(_) => break, // poll error: no reply
        }
    }
    false
}

// ---------- the UI task -------------------------------------------------------

async fn ui_task(
    cfg: Config,
    client: Arc<BackendClient>,
    session: Session,
    loop_: Arc<tokio::sync::Mutex<AgentLoop>>,
    ui_tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    mut ui_rx: tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
    mut warn_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    kitty: bool,
    mut term: Terminal<CrosstermBackend<std::io::Stdout>>,
) {
    let cancel_tx = loop_.lock().await.cancel_sender();
    let mut app = App::new(cfg, client, session, loop_, cancel_tx, kitty);
    let mut events = EventStream::new();

    loop {
        // 1. drain pending UI events (agent tasks -> UI)
        while let Ok(ev) = ui_rx.try_recv() {
            app.on_event(ev);
        }
        // 1b. runtime warnings (git/session/backend via logmsg) — rendered
        // in the conversation instead of spilling over the alt screen
        while let Ok(w) = warn_rx.try_recv() {
            app.on_event(UiEvent::Warn(w));
        }
        if app.quitting {
            break;
        }

        // 2. one terminal event, or the tick timer (status-bar clock)
        let maybe_ev: Option<Event> = tokio::select! {
            e = events.next() => match e {
                Some(Ok(ev)) => Some(ev),
                _ => None,
            },
            _ = tokio::time::sleep(TICK) => None,
        };
        let Some(ev) = maybe_ev else {
            // tick: redraw for the status-bar elapsed timer
            let _ = term.draw(|f| draw_frame(f, &app));
            continue;
        };
        if let Event::Key(key) = ev {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if app.modal.is_some() {
                handle_modal_key(&mut app, key.code, &ui_tx);
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && key.code == KeyCode::Char('c')
            {
                // CR_01/6: Ctrl-C NEVER quits — it only stops the running
                // turn. Idle: a hint instead.
                if app.working {
                    app.stop_turn();
                } else {
                    app.on_event(UiEvent::Info(
                        "nothing running — /quit to exit".into(),
                    ));
                }
            } else if key.modifiers.contains(KeyModifiers::SHIFT)
                && key.code == KeyCode::Enter
            {
                // CR_01/3: Shift+Enter inserts a newline. With
                // kbd_flags() the kitty spec's Enter/Tab/Backspace
                // "still legacy bytes" carve-out is overridden (report all
                // keys as escape codes), so Shift+Enter arrives as
                // `CSI 13;2u` -> Enter + SHIFT, while plain Enter is
                // `CSI 13u` -> Enter. This branch also catches any
                // terminal that natively reports shifted Enter. On a
                // terminal without kitty support both arrive as a bare
                // `\r` (indistinguishable) and this branch stays dormant
                // there — Ctrl-J (below) is the universal newline.
                app.insert_input("\n");
            } else if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('j' | 'J'))
            {
                // Ctrl-J = universal newline. Legacy terminals cannot
                // distinguish Shift+Enter from Enter (both are a bare
                // `\r`), and the kitty protocol doesn't reach every
                // terminal (tmux passthrough off, xterm, stock VTE), so
                // Ctrl-J is the guaranteed way to insert a newline. Both
                // the legacy byte (\x0a) and the kitty form (CSI 106;5u)
                // decode to Ctrl + Char('j'); the 'J' case guards against
                // any terminal that reports the shifted codepoint.
                app.insert_input("\n");
            } else if key.code == KeyCode::Enter {
                let value = std::mem::take(&mut app.input);
                app.input_pos = 0;
                let value = value.trim();
                if value.is_empty() {
                    continue;
                }
                if let Some(rest) = value.strip_prefix('/') {
                    let (cmd, arg) = match rest.split_once(' ') {
                        Some((c, a)) => (c.trim(), a.trim()),
                        None => (rest, ""),
                    };
                    let cmd = cmd.to_lowercase();
                    app.dispatch(&cmd, arg, &ui_tx).await;
                } else if app.working {
                    app.on_event(UiEvent::Info(
                        "already running — Ctrl-C to stop it".into(),
                    ));
                } else {
                    app.start_turn(value, &ui_tx).await;
                }
            } else {
                match key.code {
                    KeyCode::Backspace => {
                        app.backspace_input();
                    }
                    KeyCode::Delete => {
                        app.delete_input();
                    }
                    KeyCode::Left => {
                        app.move_input(-1);
                    }
                    KeyCode::Right => {
                        app.move_input(1);
                    }
                    KeyCode::PageUp => {
                        app.scroll_up(10);
                    }
                    KeyCode::PageDown => {
                        app.scroll_down(10);
                    }
                    KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        app.scroll_up(1);
                    }
                    KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        app.scroll_down(1);
                    }
                    KeyCode::Char(c) => {
                        app.insert_input(&input_char(c, key.modifiers, key.state).to_string());
                    }
                    _ => {}
                }
            }
        } else if let Event::Mouse(m) = ev {
            // CR_01/2: mouse-wheel scrolling of the conversation
            // (left button just focuses the input — nothing else needed).
            if m.kind == crossterm::event::MouseEventKind::ScrollUp {
                app.scroll_up(3);
            } else if m.kind == crossterm::event::MouseEventKind::ScrollDown {
                app.scroll_down(3);
            }
        }
        if app.quitting {
            break;
        }
        let _ = term.draw(|f| draw_frame(f, &app));
    }

    // quit: unblock a waiting approval so the turn task can wind down
    app.answer_approval(None);
}

/// Key handling while a modal is open. Esc closes (approval: denies;
/// model: the in-flight load/unload, if any, keeps running in the
/// background). Nothing here can quit the app (CR_01/6).
fn handle_modal_key(
    app: &mut App,
    code: KeyCode,
    ui_tx: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
) {
    // Kitty "report alternate keys" decodes a typed `Y` to Char('Y')
    // (no SHIFT bit), so the char match must be case-insensitive.
    // (Shared borrow only — the arms call back into `app` mutably; the
    // model modal delegates to `model_modal_key`, which borrows the
    // modal fields itself.)
    match &app.modal {
        Some(Modal::Approval { .. }) => match code {
            KeyCode::Char(c) if c.to_ascii_lowercase() == 'y' => app.answer_approval(Some("y")),
            KeyCode::Char(c) if c.to_ascii_lowercase() == 'a' => app.answer_approval(Some("a")),
            KeyCode::Char(c) if c.to_ascii_lowercase() == 'n' => app.answer_approval(Some("n")),
            KeyCode::Esc | KeyCode::Enter => app.answer_approval(None),
            _ => {}
        },
        Some(Modal::Model { .. }) => model_modal_key(app, code, ui_tx),
        None => {}
    }
}

/// Model manager keys. Esc closes (a running load/unload keeps going in
/// the background and reports to the conversation on completion).
fn model_modal_key(
    app: &mut App,
    code: KeyCode,
    ui_tx: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
) {
    match &mut app.modal {
        Some(Modal::Model { models, sel, .. }) => match code {
            KeyCode::Esc => app.modal = None,
            KeyCode::Up if !models.is_empty() => {
                *sel = if *sel == 0 { models.len() - 1 } else { *sel - 1 };
            }
            KeyCode::Down if !models.is_empty() => {
                *sel = (*sel + 1) % models.len();
            }
            // Enter switches to the selected model (persisted). The
            // backend loads unloaded models on first use anyway; `l`
            // makes it explicit and immediate.
            KeyCode::Enter if *sel < models.len() => {
                let picked = models[*sel].id.clone();
                app.modal = None;
                app.switch_model(&picked);
            }
            // number keys: select that row AND switch (the legacy
            // one-key flow, kept for muscle memory)
            KeyCode::Char(d) if d.is_ascii_digit() => {
                let i = d.to_digit(10).unwrap_or(0) as usize - 1;
                if i < models.len() {
                    let picked = models[i].id.clone();
                    app.modal = None;
                    app.switch_model(&picked);
                }
            }
            // l: load the selected model into the backend (one op at a
            // time)
            KeyCode::Char(c) if c.to_ascii_lowercase() == 'l' && *sel < models.len() => {
                let name = models[*sel].id.clone();
                if app.model_op.is_none() {
                    app.model_op(name, true, ui_tx);
                } else {
                    app.on_event(UiEvent::Info(
                        "a model operation is already in progress".into(),
                    ));
                }
            }
            // u: unload the selected model
            KeyCode::Char(c) if c.to_ascii_lowercase() == 'u' && *sel < models.len() => {
                let name = models[*sel].id.clone();
                if app.model_op.is_none() {
                    app.model_op(name, false, ui_tx);
                } else {
                    app.on_event(UiEvent::Info(
                        "a model operation is already in progress".into(),
                    ));
                }
            }
            // r: re-scan the backend's model sources (?reload=1)
            KeyCode::Char(c) if c.to_ascii_lowercase() == 'r' => {
                let client = app.client.clone();
                let tx = ui_tx.clone();
                tokio::spawn(async move {
                    match client.refresh_models().await {
                        Ok(m) => {
                            let _ = tx.send(UiEvent::Models(m));
                        }
                        Err(e) => {
                            let _ = tx.send(UiEvent::ModelsFailed(e.to_string()));
                        }
                    }
                });
            }
            _ => {}
        },
        _ => {}
    }
}

fn draw_frame(f: &mut Frame, app: &App) {
    f.render_widget(Clear, f.area());
    let area = f.area();
    // The input block grows with the buffer's VISIBLE rows (hard newlines
    // plus soft wraps of too-long lines, CR_01/3), capped at
    // MAX_INPUT_LINES, plus the hint row and the trailing separator row.
    let input_rows = input_block_lines(&app.input, area.width as usize) as u16;
    let pieces = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // pinned header (CR_01/1)
            Constraint::Min(3),   // conversation
            Constraint::Length(1), // status bar
            Constraint::Length(input_rows + 2), // input lines + hint + separator
        ])
        .split(area);
    if pieces.len() != 4 {
        return;
    }
    let (header_area, conv, status, input_area) =
        (pieces[0], pieces[1], pieces[2], pieces[3]);

    // -- pinned header: session · model · tools (never scrolls, CR_01/1)
    f.render_widget(
        Paragraph::new(app.header.clone()).style(Style::default().fg(Color::Black)),
        header_area,
    );

    // -- conversation: wrap the document, compute the scroll offset
    let width = conv.width.max(1) as usize;
    let counts = wrapped_counts(&app.doc, width);
    let total = app.base + counts.iter().sum::<usize>();
    let visible = conv.height.max(1) as usize;
    let offset = scroll_offset(app.user_scroll, total, visible);
    // drop whole leading lines beyond the offset; the remainder is the
    // Paragraph scroll (it scrolls within the rendered, wrapped buffer)
    let mut start = 0usize;
    let mut acc = 0usize;
    for (i, &c) in counts.iter().enumerate() {
        if acc + c > offset {
            start = i;
            break;
        }
        acc += c;
        start = i + 1;
    }
    let scroll_in = (offset - acc) as u16;
    let text: Vec<Line> = app.doc[start..].iter().cloned().collect();
    let para = Paragraph::new(Text::from(text))
        .wrap(Wrap { trim: false })
        .scroll((scroll_in, 0));
    f.render_widget(para, conv);

    // -- status bar
    let mut status_line = Line::default();
    if app.working {
        let elapsed = app
            .working_since
            .map(|t| format!(" {:3.0}s", t.elapsed().as_secs_f64()))
            .unwrap_or_default();
        status_line = status_line + Span::styled(
            format!(" {}{elapsed} ", app.working_what),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow),
        );
        status_line = status_line + Span::raw("  ");
    }
    status_line = status_line
        + Span::styled(
            " model ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        + Span::raw(format!(
            "{}  ·  prompt tokens {}  ·  completion tokens {}",
            app.client.model(),
            app.tokens,
            app.completion_tokens
        ));
    if let Some(c) = &app.commit {
        status_line = status_line
            + Span::styled(format!("  ·  workspace@{c}"), Style::default().fg(Color::Blue));
    }
    f.render_widget(Paragraph::new(status_line), status);

    // -- input (CR_01/3): one visible row per hard buffer line, SOFT-WRAPPED
    //    when a line is too long for the box + blinking block cursor
    let ipieces = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(input_rows), Constraint::Min(1)])
        .split(input_area);
    let input_lines = render_input(app, ipieces[0].width as usize);
    f.render_widget(
        Paragraph::new(Text::from(input_lines)).wrap(Wrap { trim: false }),
        ipieces[0],
    );
    // hint row (newline key depends on the startup kitty probe result)
    let newline_key = if app.kitty { "shift+enter" } else { "ctrl-j" };
    let hint = Line::from(format!(
        "{newline_key} newline · PgUp/PgDn, Shift+↑↓ or wheel: scroll · /help"
    ))
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(Paragraph::new(hint), ipieces[1]);

    // -- modal on top
    if let Some(m) = &app.modal {
        draw_modal(f, area, m, app);
    }
}

/// Visible lines for a submitted user request: "❯ " marker (bold
/// green) on the first line, 2-space indent on the continuation lines
/// (plain green) — the user's request must read apart from the agent's
/// "▸ "-marked default-color answers and the dim tool output.
pub(crate) fn user_request_lines(prompt: &str) -> Vec<Line<'static>> {
    let user_style = Style::default().fg(Color::Green);
    prompt
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                Line::from(Span::styled(
                    format!("❯ {line}"),
                    user_style.add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(Span::styled(format!("  {line}"), user_style))
            }
        })
        .collect()
}

/// Cap on how many VISIBLE rows the input box renders at once; beyond it
/// the visible window scrolls to keep the caret's row in view.
pub(crate) const MAX_INPUT_LINES: usize = 5;

/// Number of rows the input block occupies for this buffer at the given
/// width: each hard line of the buffer wraps into
/// `ceil(len / (width - 2))` visible rows (see `render_input_lines`).
/// One helper for both the layout and the conversation-height math so
/// they agree.
pub(crate) fn input_block_lines(input: &str, width: usize) -> usize {
    let avail = width.saturating_sub(2).max(1);
    let rows = input
        .split('\n')
        .map(|seg| {
            let n = seg.chars().count();
            if n == 0 { 1 } else { n.div_ceil(avail) }
        })
        .sum::<usize>();
    rows.clamp(1, MAX_INPUT_LINES)
}

/// Build the visible input lines. Each hard line of the buffer
/// (Shift+Enter/Ctrl-J newlines, CR_01/3) is SOFT-WRAPPED into as many
/// visible rows as the width allows, so a too-long line stays fully
/// visible and its end (and any newline after it) is never hidden — the
/// old clamp-to-width + "…" sliding window swallowed long lines and made
/// newlines look like they didn't register. Prompt marker on the first
/// visible row, 2-space indent on the continuations, blinking block
/// cursor at the caret. When the buffer has more than `MAX_INPUT_LINES`
/// visible rows the window is scrolled so the caret's row is in view.
fn render_input(app: &App, width: usize) -> Vec<Line<'static>> {
    let blink_on = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_millis() < 450)
        .unwrap_or(true);
    render_input_lines(&app.input, app.input_pos, width, blink_on)
}

/// Pure core of [`render_input`], exposed for unit tests.
pub(crate) fn render_input_lines(
    input: &str,
    pos: usize,
    width: usize,
    blink_on: bool,
) -> Vec<Line<'static>> {
    let prompt = "> ";
    let avail = width.saturating_sub(prompt.chars().count()).max(1);
    let total = input.chars().count();
    let pos = pos.min(total);

    // soft-wrap each hard line into `avail`-char visible rows (the row
    // prefix "> " / "  " takes the other 2 columns)
    let hard: Vec<Vec<char>> = input.split('\n').map(|s| s.chars().collect()).collect();
    let mut rows: Vec<Vec<char>> = Vec::new();
    let mut row_of_line = Vec::with_capacity(hard.len());
    for chars in &hard {
        row_of_line.push(rows.len());
        let n = chars.len();
        if n == 0 {
            rows.push(Vec::new());
        } else {
            for s in (0..n).step_by(avail) {
                rows.push(chars[s..(s + avail).min(n)].to_vec());
            }
        }
    }

    // which hard line / column the caret sits in, folded into visible rows
    let (mut caret_line, mut caret_col) = (0usize, 0usize);
    for c in input.chars().take(pos) {
        if c == '\n' {
            caret_line += 1;
            caret_col = 0;
        } else {
            caret_col += 1;
        }
    }
    caret_line = caret_line.min(hard.len().saturating_sub(1));
    let n = hard[caret_line].len();
    let (caret_row, caret_col) = if caret_col == n && n > 0 {
        // end of the line: park on its LAST wrapped row, past its end
        // (a naive div_ceil would spill into the next line's row)
        let lc = n.div_ceil(avail);
        (row_of_line[caret_line] + lc - 1, n - (lc - 1) * avail)
    } else {
        (row_of_line[caret_line] + caret_col.div_ceil(avail), caret_col % avail)
    };

    // vertical window: keep the caret's visible row in view
    let start = if caret_row >= MAX_INPUT_LINES {
        caret_row - MAX_INPUT_LINES + 1
    } else {
        0
    };

    let mut out = Vec::with_capacity(rows.len() - start);
    for (i, row) in rows[start..].iter().take(MAX_INPUT_LINES).enumerate() {
        let is_caret = start + i == caret_row;
        let body: String = row.iter().collect();
        let n = body.chars().count();

        let mut spans: Vec<Span> = Vec::new();
        if i == 0 {
            spans.push(Span::styled(
                prompt.to_string(),
                Style::default().fg(Color::Green),
            ));
        } else {
            spans.push(Span::raw("  ".to_string())); // continuation indent
        }
        if is_caret {
            let col = caret_col.min(n);
            spans.push(Span::raw(body.chars().take(col).collect::<String>()));
            if blink_on {
                let ch = if col < n {
                    body.chars().nth(col).unwrap()
                } else {
                    ' '
                };
                spans.push(Span::styled(
                    ch.to_string(),
                    Style::default().bg(Color::White).fg(Color::Black),
                ));
            }
            let skip = col + usize::from(blink_on);
            spans.push(Span::raw(body.chars().skip(skip).collect::<String>()));
        } else {
            spans.push(Span::raw(body));
        }
        // keep the hint visible when the input is empty
        if input.is_empty() {
            spans.push(Span::styled(
                "type a request…".to_string(),
                Style::default().fg(Color::DarkGray),
            ));
        }
        out.push(Line::from(spans));
    }
    out
}

/// Model manager rows: `>  1  ● name  [status]` — `>` marks the selected
/// row (arrows), `●` the current model, and the colored `[status]` tag
/// comes from the backend (`[remote]` when it reports no status, i.e. a
/// plain OpenAI endpoint). Pure so it is unit-testable.
pub(crate) fn model_modal_lines(
    models: &[ModelInfo],
    current: &str,
    sel: usize,
) -> Vec<Line<'static>> {
    models
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let tag = if m.failed { "failed" } else { m.status.label() };
            Line::from(vec![
                Span::styled(
                    format!("{} {:>2} ", if i == sel { ">" } else { " " }, i + 1),
                    if i == sel {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::raw(format!(
                    "{}{}  ",
                    if m.id == current { "● " } else { "   " },
                    m.id
                )),
                Span::styled(format!("[{tag}]"), model_status_style(m.status, m.failed)),
            ])
        })
        .collect()
}

fn model_status_style(st: ModelStatus, failed: bool) -> Style {
    let c = if failed {
        Color::Red
    } else {
        match st {
            ModelStatus::Loaded => Color::Green,
            ModelStatus::Loading | ModelStatus::Downloading => Color::Yellow,
            ModelStatus::Sleeping => Color::Cyan,
            ModelStatus::Downloaded => Color::Blue,
            ModelStatus::Unloaded => Color::DarkGray,
            ModelStatus::Unknown => Color::Gray,
        }
    };
    Style::default().fg(c)
}

fn draw_modal(f: &mut Frame, area: Rect, m: &Modal, app: &App) {
    let (title, body_lines, footer): (String, Vec<Line<'static>>, String) = match m {
        Modal::Approval { name, preview, .. } => {
            let body: Vec<Line<'static>> = preview
                .lines()
                .take(PREVIEW_CAP / 10)
                .map(|l| Line::from(l.to_string()))
                .collect();
            (
                format!("approval requested — {name}"),
                body,
                "[y]es once   [a]lways allow   [n] / Esc — deny".to_string(),
            )
        }
        Modal::Model { models, sel, note } => {
            let mut body = model_modal_lines(models, &app.client.model(), *sel);
            if let Some(n) = note {
                body.push(Line::from(Span::styled(
                    format!("  {n}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
            }
            let footer = if let Some(op) = &app.model_op {
                format!("… {op} in progress — Esc closes, the operation continues")
            } else {
                "↑↓ move · enter switch · l load · u unload · r refresh · Esc close"
                    .to_string()
            };
            (
                format!("models (current: {})", app.client.model()),
                body,
                footer,
            )
        }
    };
    let inner_w = area.width.saturating_sub(6);
    let max_body = area.height.saturating_sub(8);
    let title_w = est_width(&title);
    let footer_w = est_width(&footer);
    let body_w = body_lines.iter().map(|l| line_width(l)).max().unwrap_or(0);
    let w = (title_w.max(footer_w).max(body_w).min(inner_w as usize) + 4).max(24) as u16;
    let w = w.min(area.width.saturating_sub(2));
    // border(2) + title(1) + body + footer(1)
    let h = body_lines.len().min(max_body as usize) as u16 + 4;
    let h = h.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w).saturating_sub(1)) / 2;
    let y = area.y + (area.height.saturating_sub(h).saturating_sub(1)) / 2;
    let box_area = Rect { x, y, width: w, height: h };
    // The border is rendered FIRST and the content laid out inside it:
    // drawing it last (the old code) painted the title and footer rows
    // right over them, hiding e.g. the approval modal's y/a/n hint.
    f.render_widget(
        Block::bordered().border_style(Style::default().fg(Color::Yellow)),
        box_area,
    );
    let inner = Rect {
        x: x.saturating_add(1),
        y: y.saturating_add(1),
        width: w.saturating_sub(2),
        height: h.saturating_sub(2),
    };
    let mpieces = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);
    if mpieces.len() != 3 {
        return;
    }
    let (head, body, foot) = (mpieces[0], mpieces[1], mpieces[2]);
    f.render_widget(
        Paragraph::new(title).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        head,
    );
    f.render_widget(Paragraph::new(body_lines).wrap(Wrap { trim: false }), body);
    f.render_widget(
        Paragraph::new(footer).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Gray),
        ),
        foot,
    );
}

// ---------- entry point -------------------------------------------------------

/// Build the shared loop (+ its lock-free cancel handle) for a session.
fn make_loop(
    cfg: &Config,
    client: &Arc<BackendClient>,
    session: Session,
    ui_tx: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
) -> (Arc<tokio::sync::Mutex<AgentLoop>>, tokio::sync::watch::Sender<bool>) {
    let (ask, emit) = make_seams(ui_tx);
    let loop_ =
        AgentLoop::new(cfg.clone(), client.clone(), session, Some(ask), Some(emit));
    let cancel_tx = loop_.cancel_sender();
    (Arc::new(tokio::sync::Mutex::new(loop_)), cancel_tx)
}

/// Run the TUI (blocking). Normal quit restores the terminal and returns.
pub fn run(cfg: Config) -> anyhow::Result<()> {
    crate::tools::register_all();
    // Terminal setup + kitty detection FIRST, before seeding: the probe's
    // short stdin read window (the kitty handshake) must be over before a
    // user is likely to start typing, and seeding (git init / commits) is
    // what makes startup slow. The probe only writes invisible CSI
    // sequences, so running it on the real terminal (pre-alt-screen) is
    // fine.
    let (term, kitty) = prepare_terminal()?;
    // Runtime warnings route into the conversation; register the sink
    // before seeding so even seed errors land in the UI, not on the
    // alternate screen.
    let (warn_tx, warn_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    crate::logmsg::set_sink(Some(warn_tx));
    if crate::state::seed::seed(&cfg) {
        // config file was just written; reload like the headless path does
        let cfg = crate::config::load_config();
        return run_inner(cfg, warn_rx, term, kitty);
    }
    run_inner(cfg, warn_rx, term, kitty)
}

/// Enter raw mode + alternate screen + mouse capture, install the panic
/// restore hook, run the kitty keyboard-protocol probe and enable the
/// kitty flags ONLY if the terminal answered. Returns the terminal and
/// whether Shift+Enter is distinguishable from Enter (kitty mode).
fn prepare_terminal() -> anyhow::Result<(Terminal<CrosstermBackend<std::io::Stdout>>, bool)> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    // mouse wheel scrolling (CR_01/2); restored on every exit path below
    stdout.execute(EnableMouseCapture)?;
    // restore the terminal on panic too (raw mode would otherwise stick)
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut so = std::io::stdout();
        let _ = so.execute(DisableMouseCapture);
        let _ = so.execute(Show);
        let _ = disable_raw_mode();
        // restore keyboard mode before leaving the alt screen (kitty docs)
        let _ = so.execute(PopKeyboardEnhancementFlags);
        let _ = so.execute(LeaveAlternateScreen);
        prev(info);
    }));
    // Synchronous, bounded by 100 ms — see `kitty_probe` for why this
    // must NOT run on a throwaway tokio runtime (the runtime drop hung
    // startup on a black screen until the first keystroke).
    let kitty = kitty_probe();
    if kitty {
        stdout.execute(PushKeyboardEnhancementFlags(kbd_flags()))?;
        let _ = stdout.flush();
    }
    let backend = CrosstermBackend::new(stdout);
    let term = Terminal::new(backend)?;
    Ok((term, kitty))
}

fn run_inner(
    cfg: Config,
    warn_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    term: Terminal<CrosstermBackend<std::io::Stdout>>,
    kitty: bool,
) -> anyhow::Result<()> {
    let client = Arc::new(BackendClient::new(&cfg));
    let session = Session::create(DEFAULT_AGENT, &cfg.backend.model, None);
    // the app keeps its own Session view of the same JSONL file
    let session_for_app = Session::open(&session.path);

    // one UI channel shared by the loop seams (sender) and the UI task
    // (receiver)
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
    let (loop_, _cancel_tx) = make_loop(&cfg, &client, session, &ui_tx);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(ui_task(
        cfg,
        client,
        session_for_app,
        loop_,
        ui_tx,
        ui_rx,
        warn_rx,
        kitty,
        term,
    ));

    disable_raw_mode()?;
    crate::logmsg::set_sink(None);
    let mut stdout = std::io::stdout();
    stdout.execute(DisableMouseCapture)?;
    stdout.execute(Show)?;
    // restore keyboard mode while still in the alt screen (kitty docs),
    // then leave the alt screen
    stdout.execute(PopKeyboardEnhancementFlags)?;
    stdout.execute(LeaveAlternateScreen)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref().to_string())
            .collect()
    }

    fn doc_text(app: &App) -> Vec<String> {
        app.doc.iter().map(line_text).collect()
    }

    // ---- scroll math ----------------------------------------------------

    #[test]
    fn scroll_offset_sticks_to_bottom_and_pages_up() {
        // 30 logical lines, 10 visible: the bottom offset is 20
        assert_eq!(scroll_offset(None, 30, 10), 20);
        // 10 lines from the bottom (PageUp) -> offset 10 — NOT "10 from
        // the top", which is what the old code rendered
        assert_eq!(scroll_offset(Some(10), 30, 10), 10);
        // clamped at the top of the document
        assert_eq!(scroll_offset(Some(50), 30, 10), 0);
        // document smaller than the viewport: nothing to scroll
        assert_eq!(scroll_offset(None, 5, 10), 0);
        assert_eq!(scroll_offset(Some(50), 5, 10), 0);
    }

    // ---- kitty probe response parsing ------------------------------------

    #[test]
    fn parse_kitty_probe_response_accepts_flag_replies() {
        assert_eq!(parse_kitty_probe_response(b"\x1b[?1;13u"), Some(13));
        assert_eq!(parse_kitty_probe_response(b"\x1b[?1;1u"), Some(1));
        // a reply buried after stray bytes still parses
        assert_eq!(parse_kitty_probe_response(b"junk\x1b[?1;13u"), Some(13));
        // everything else (junk, partial, other replies): None
        assert_eq!(parse_kitty_probe_response(b""), None);
        assert_eq!(parse_kitty_probe_response(b"a\x1b"), None);
        assert_eq!(parse_kitty_probe_response(b"\x1b[?1;13"), None); // no 'u' yet
        assert_eq!(parse_kitty_probe_response(b"\x1b[?6c"), None); // DA reply
        assert_eq!(parse_kitty_probe_response(b"\x1b[?1;u"), None); // no digits
    }

    // ---- model modal (Phase 8: load / unload / status) --------------------

    fn info(id: &str, status: ModelStatus) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            status,
            failed: false,
            source: None,
            context_length: None,
        }
    }

    #[test]
    fn model_modal_lines_mark_selection_current_and_status() {
        let models = vec![
            info("a", ModelStatus::Loaded),
            info("b", ModelStatus::Unloaded),
            info("c", ModelStatus::Sleeping),
        ];
        let lines = model_modal_lines(&models, "b", 1);
        assert_eq!(lines.len(), 3);
        // row: marker + number, then the current-model dot, then the id
        assert!(line_text(&lines[0]).starts_with("   1 "), "{}", line_text(&lines[0]));
        assert!(line_text(&lines[0]).contains("a"));
        assert!(line_text(&lines[0]).contains("[loaded]"));
        // selected row: ">" marker + bold
        assert!(line_text(&lines[1]).starts_with(">  2 "), "{}", line_text(&lines[1]));
        assert!(line_text(&lines[1]).contains("● b"));
        assert!(line_text(&lines[1]).contains("[unloaded]"));
        assert_eq!(lines[1].spans[0].style, Style::default().add_modifier(Modifier::BOLD));
        assert_eq!(lines[0].spans[0].style, Style::default());
        assert!(line_text(&lines[2]).contains("[sleeping]"));
    }

    #[test]
    fn model_modal_lines_show_failed_tag_and_remote() {
        let mut m = info("bad", ModelStatus::Unloaded);
        m.failed = true;
        let lines = model_modal_lines(&[m], "bad", 0);
        assert!(line_text(&lines[0]).contains("[failed]"), "{}", line_text(&lines[0]));
        // a backend without status support shows [remote]
        let lines = model_modal_lines(&[info("gpt", ModelStatus::Unknown)], "gpt", 0);
        assert!(line_text(&lines[0]).contains("[remote]"), "{}", line_text(&lines[0]));
    }

    #[test]
    fn models_event_opens_modal_on_current_and_updates_in_place() {
        let mut app = test_app();
        app.on_event(UiEvent::Models(vec![
            info("x", ModelStatus::Loaded),
            info("test-model", ModelStatus::Unloaded),
        ]));
        // fresh open: selection starts on the current model
        match &app.modal {
            Some(Modal::Model { sel, note, .. }) => {
                assert_eq!(*sel, 1);
                assert!(note.is_none());
            }
            _ => panic!("modal should be open"),
        }
        // a re-query while open updates in place and clamps the selection
        app.on_event(UiEvent::Models(vec![info("y", ModelStatus::Loaded)]));
        match &app.modal {
            Some(Modal::Model { sel, .. }) => assert_eq!(*sel, 0),
            _ => panic!("modal should stay open"),
        }
        // an op refresh with the modal closed is ignored (the result
        // lands in the conversation via ModelOpDone instead)
        app.modal = None;
        app.on_event(UiEvent::ModelsRefresh(vec![info("z", ModelStatus::Loaded)]));
        assert!(app.modal.is_none());
        // an empty fresh query surfaces an error line, no modal
        app.on_event(UiEvent::Models(Vec::new()));
        assert!(app.modal.is_none());
        assert!(doc_text(&app).contains(&"backend returned no models".to_string()));
    }

    #[test]
    fn model_op_done_updates_note_or_conversation() {
        let mut app = test_app();
        app.on_event(UiEvent::Models(vec![info("x", ModelStatus::Unloaded)]));
        app.model_op = Some("x".into());
        app.on_event(UiEvent::ModelOpDone {
            name: "x".into(),
            verb: "loaded".into(),
            ok: true,
            detail: String::new(),
        });
        // modal open -> result becomes its note, the op marker clears
        match &app.modal {
            Some(Modal::Model { note, .. }) => assert_eq!(note.as_deref(), Some("loaded x")),
            _ => panic!("modal should stay open"),
        }
        assert!(app.model_op.is_none());
        // modal closed -> a conversation line instead
        app.modal = None;
        app.model_op = Some("y".into());
        app.on_event(UiEvent::ModelOpDone {
            name: "y".into(),
            verb: "unloading".into(),
            ok: false,
            detail: "HTTP 400: model is not running".into(),
        });
        assert!(app.model_op.is_none());
        let text = doc_text(&app);
        assert!(
            text.contains(&"unloading y failed: HTTP 400: model is not running".to_string()),
            "{text:?}"
        );
    }

    // ---- section separators (message / tool / thinking spacing) ----------

    fn test_app() -> App {
        let dir = std::env::temp_dir().join(format!(
            "harness-tui-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = Config::default();
        cfg.workspace.path = dir.to_string_lossy().to_string();
        cfg.backend.model = "test-model".into();
        let client = Arc::new(BackendClient::new(&cfg));
        let session = Session::create("agent", "test-model", Some(&dir.join("sessions")));
        let session_for_app = Session::open(&session.path);
        let loop_ = AgentLoop::new(cfg.clone(), client.clone(), session, None, None);
        let cancel_tx = loop_.cancel_sender();
        App::new(
            cfg,
            client,
            session_for_app,
            Arc::new(tokio::sync::Mutex::new(loop_)),
            cancel_tx,
            false,
        )
    }

    #[test]
    fn sections_are_separated_by_blank_lines() {
        let mut app = test_app();
        app.on_event(UiEvent::Text("hello".into()));
        app.on_event(UiEvent::Text(" world".into())); // same stream: extends, no blank
        app.on_event(UiEvent::Tool {
            name: "run_command".into(),
            args: json!({ "cmd": "ls" }),
        });
        // result stays attached to its call (one "tool" block)
        app.on_event(UiEvent::ToolResult { ok: true, result: "ok".into() });
        app.on_event(UiEvent::Reasoning("hmm".into()));
        app.on_event(UiEvent::Text("answer".into()));
        app.on_event(UiEvent::Warn("git: boom\nsecond line".into()));

        assert_eq!(
            doc_text(&app),
            vec![
                "▸ hello world",
                "",
                "⚙ run_command {\"cmd\":\"ls\"}",
                "  ✓ ok",
                "",
                "hmm",
                "",
                "▸ answer",
                "",
                "⚠ git: boom",
                "  │ second line",
            ]
        );
    }

    #[test]
    fn no_blank_line_before_the_first_line() {
        let mut app = test_app();
        app.on_event(UiEvent::Text("first".into()));
        app.on_event(UiEvent::Tool {
            name: "t".into(),
            args: Value::Null,
        });
        assert_eq!(
            doc_text(&app),
            vec!["▸ first", "", "⚙ t {\"raw\":null}"]
        );
    }

    #[test]
    fn clear_doc_resets_section_state() {
        let mut app = test_app();
        app.on_event(UiEvent::Text("old".into()));
        app.clear_doc();
        app.on_event(UiEvent::Text("new".into()));
        // no leading blank after a clear
        assert_eq!(doc_text(&app), vec!["▸ new"]);
    }

    #[test]
    fn render_input_empty_shows_hint() {
        let l = render_input_lines("", 0, 80, true)[0].clone();
        let t = line_text(&l);
        assert!(t.starts_with("> "), "{t}");
        assert!(t.contains("type a request"), "{t}");
    }

    #[test]
    fn render_input_short_fits_whole_line() {
        let l = render_input_lines("hello", 5, 80, false)[0].clone();
        let t = line_text(&l);
        assert!(t.starts_with("> hello"), "{t}");
        assert!(!t.contains('…'), "no ellipsis expected: {t}");
    }

    #[test]
    fn render_input_long_line_wraps_and_stays_fully_visible() {
        // the reported bug: a too-long line used to clamp to one row behind
        // a "…" sliding window, hiding most of the text (and any newline
        // after it). Now it soft-wraps; the whole tail of the line is
        // visible, no ellipsis.
        let input: String = "abcdefghij".repeat(10); // 100 chars
        let lines = render_input_lines(&input, input.chars().count(), 20, false);
        assert_eq!(lines.len(), MAX_INPUT_LINES, "{lines:?}");
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        for t in &texts {
            assert!(!t.contains('…'), "no ellipsis anymore: {t}");
            assert!(t.chars().count() <= 20, "row fits the box: {t}");
        }
        // prompt on the first visible row, indent on the rest
        assert!(texts[0].starts_with("> "), "{:?}", texts[0]);
        for t in &texts[1..] {
            assert!(t.starts_with("  "), "continuation indent: {t}");
        }
        // the visible bodies join back to the (clamped) tail of the line
        let body: String = texts
            .iter()
            .map(|t| t.chars().skip(2).collect::<String>())
            .collect();
        assert_eq!(body, input[18..], "no char of the line is hidden");
    }

    #[test]
    fn render_input_newline_after_long_line_is_visible() {
        // the reported bug: with the old one-row-per-line clamp, the
        // newline typed at the end of an over-long line was invisible.
        // The wrapped line must show its real end, then the new row.
        let input = format!("{}\ny", "x".repeat(40)); // 40 x's + \n + y
        let lines = render_input_lines(&input, input.chars().count(), 20, false);
        // 40 x's wrap into 3 rows (18+18+4), plus the "y" row
        assert_eq!(lines.len(), 4, "{lines:?}");
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts[0], format!("> {}", "x".repeat(18)));
        assert_eq!(texts[1], format!("  {}", "x".repeat(18)));
        assert_eq!(texts[2], format!("  {}", "x".repeat(4)));
        assert_eq!(texts[3], "  y", "the newline shows as its own row");
    }

    #[test]
    fn render_input_caret_midline_lands_on_wrapped_row() {
        // caret in the middle of a too-long line: it sits on the wrapped
        // row the char falls in, and that row is in the visible window
        let input: String = "abcdefghij".repeat(10); // 100 chars
        let lines = render_input_lines(&input, 50, 20, true); // blink on
        assert_eq!(lines.len(), MAX_INPUT_LINES, "{lines:?}");
        // width 20 -> avail 18: char 50 is on wrapped row 3 (chars 54..72)
        let caret_line = &lines[3];
        // prefix (2) + take(14) + block (1) + rest (3) = 4 spans
        assert_eq!(caret_line.spans.len(), 4, "{caret_line:?}");
        let t = line_text(caret_line);
        assert!(t.starts_with("  "), "{t}");
        assert_eq!(t.chars().count(), 20, "{t}");
    }

    #[test]
    fn render_input_newline_becomes_own_row() {
        // hard newlines render as real line breaks in the box (CR_01/3):
        // one row per buffer line, prompt on the first, indented continuations
        let lines = render_input_lines("a\nb", 3, 40, false);
        assert_eq!(lines.len(), 2, "one row per hard line: {lines:?}");
        assert_eq!(line_text(&lines[0]), "> a");
        let second = line_text(&lines[1]);
        assert!(second.starts_with("  "), "continuation indent: {second}");
        assert!(second.ends_with('b'), "{second}");
        assert!(!second.contains('\n'));
    }

    #[test]
    fn render_input_caret_on_second_row_renders_caret_there() {
        let lines = render_input_lines("ab\ncd", 4, 40, false); // caret after "ab\n"
        assert_eq!(lines.len(), 2, "{lines:?}");
        let first = line_text(&lines[0]);
        assert!(first.ends_with("ab"), "no caret on row 1: {first}");
        let second = line_text(&lines[1]);
        assert!(second.ends_with("cd"), "caret at the start of row 2: {second}");
    }

    #[test]
    fn render_input_many_lines_window_scrolls_to_caret() {
        // 8 hard lines, caret on the last: only MAX_INPUT_LINES rows are
        // rendered, and the caret's line must be in the window
        let input = (0..8).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let pos = input.chars().count(); // caret at the very end
        let lines = render_input_lines(&input, pos, 40, false);
        assert_eq!(lines.len(), MAX_INPUT_LINES, "{lines:?}");
        let first = line_text(&lines[0]);
        assert!(first.starts_with("> line3"), "window starts at line 3: {first}");
        let last = line_text(&lines[MAX_INPUT_LINES - 1]);
        assert!(last.ends_with("line7"), "caret's line visible: {last}");
    }

    #[test]
    fn input_block_lines_grows_with_newlines_wraps_and_caps() {
        assert_eq!(input_block_lines("", 80), 1);
        assert_eq!(input_block_lines("one line", 80), 1);
        assert_eq!(input_block_lines("a\nb\nc", 80), 3);
        // a too-long line counts its wrapped rows too
        assert_eq!(input_block_lines(&"x".repeat(36), 20), 2); // avail 18
        assert_eq!(input_block_lines(&"x".repeat(100), 20), MAX_INPUT_LINES);
        let ten = (0..10).map(|_| "x").collect::<Vec<_>>().join("\n");
        assert_eq!(input_block_lines(&ten, 80), MAX_INPUT_LINES);
    }

    #[test]
    fn pretty_result_formats_json_objects() {
        let r = pretty_result(r#"{"a":1,"b":[1,2]}"#);
        assert!(r.contains("\n"), "pretty-printed across lines: {r}");
        assert!(r.contains("\"a\": 1"), "{r}");
    }

    #[test]
    fn pretty_result_passes_through_plain_text() {
        let r = pretty_result("hello world");
        assert_eq!(r, "hello world");
        let r = pretty_result("plain text with: no json");
        assert_eq!(r, "plain text with: no json");
    }

    // input_char (kitty "report alternate keys" decoding + Caps Lock)
    #[test]
    fn input_char_plain_and_kitty_shifted() {
        // legacy: raw shifted byte decodes to the char already
        assert_eq!(input_char('A', KeyModifiers::SHIFT, KeyEventState::NONE), 'A');
        // kitty alt-key: crossterm already gives the shifted char, no SHIFT bit
        assert_eq!(input_char('A', KeyModifiers::empty(), KeyEventState::NONE), 'A');
        assert_eq!(input_char('!', KeyModifiers::empty(), KeyEventState::NONE), '!');
        // non-ascii passes through
        assert_eq!(input_char('é', KeyModifiers::empty(), KeyEventState::NONE), 'é');
    }

    #[test]
    fn input_char_caps_lock_uppercases() {
        // kitty: `a` with Caps on = CSI 97;65u -> Char('a') + CAPS_LOCK,
        // no SHIFT modifier -> re-apply the shift
        assert_eq!(
            input_char('a', KeyModifiers::empty(), KeyEventState::CAPS_LOCK),
            'A'
        );
        // digit under Caps Lock stays a digit
        assert_eq!(
            input_char('1', KeyModifiers::empty(), KeyEventState::CAPS_LOCK),
            '1'
        );
    }

    #[test]
    fn input_char_ctrl_and_alt_not_touched() {
        // Caps Lock state must not interfere with other modifiers
        assert_eq!(
            input_char(
                'a',
                KeyModifiers::CONTROL,
                KeyEventState::CAPS_LOCK
            ),
            'a'
        );
        assert_eq!(
            input_char(
                'a',
                KeyModifiers::ALT,
                KeyEventState::CAPS_LOCK
            ),
            'a'
        );
        // SHIFT + CAPS (Shift held under Caps): kitty already delivered the
        // shifted alternate for the SHIFT modifier -> leave it alone
        assert_eq!(
            input_char(
                'A',
                KeyModifiers::SHIFT,
                KeyEventState::CAPS_LOCK
            ),
            'A'
        );
    }

    // ---- agent text block markers / user request display -----------------

    #[test]
    fn agent_text_marker_starts_each_block_only() {
        let mut app = test_app();
        app.on_event(UiEvent::Text("para one\npara two".into()));
        app.on_event(UiEvent::Text(" more".into())); // extends the open line
        app.on_event(UiEvent::Tool {
            name: "t".into(),
            args: Value::Null,
        });
        app.on_event(UiEvent::Reasoning("thinking".into()));
        app.on_event(UiEvent::Text("final".into()));
        assert_eq!(
            doc_text(&app),
            vec![
                "▸ para one",
                "para two more",
                "",
                "⚙ t {\"raw\":null}",
                "",
                "thinking",
                "",
                "▸ final",
            ]
        );
    }

    #[test]
    fn agent_text_continues_across_deltas_without_remarking() {
        let mut app = test_app();
        app.on_event(UiEvent::Text("abc".into()));
        // leading-newline delta: the second line continues the same block
        app.on_event(UiEvent::Text("\nnext para".into()));
        assert_eq!(doc_text(&app), vec!["▸ abc", "next para"]);
    }

    #[test]
    fn user_request_lines_mark_and_indent() {
        let lines = user_request_lines("hello\nworld");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(line_text(&lines[0]), "❯ hello");
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(Color::Green),
            "first line green"
        );
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD),
            "first line bold"
        );
        assert_eq!(line_text(&lines[1]), "  world");
        assert!(!lines[1].spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn turn_header_counts_from_loop_session_and_shows_request() {
        let mut app = test_app();
        // two completed turns on the LOOP's session (the authoritative
        // view — the app's own Session snapshot would still say 0)
        app.loop_.blocking_lock().session.record_turn_end(1, None, 10, 5);
        app.loop_.blocking_lock().session.record_turn_end(2, None, 20, 7);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
        rt.block_on(app.start_turn("do the thing\nand this too", &tx));
        let doc = doc_text(&app);
        // the header numbers the NEXT turn from the loop's live session
        assert!(doc[0].starts_with("─ turn 3 "), "{doc:?}");
        // and the user's request is displayed, marker + indent
        assert!(doc.contains(&"❯ do the thing".to_string()), "{doc:?}");
        assert!(doc.contains(&"  and this too".to_string()), "{doc:?}");
    }
}
