# AGENT.md — session continuity

Reference folder contains code that might be useful to you.
`agent/` is your workspace for temporary files and the progress todo
(it is the agent folder: only `AGENT.md` and `SKILL.md` at its top level
get injected into the system prompt — everything else is scratch and is
never injected; put volatile state like this TODO in other files).

## Project

Rust port of the Python harness in `reference/harness/`. The plan is
**`plan.md`** (root) — read it before doing anything. Progress is tracked in
**`agent/TODO.md`**.

### Hard rules (violating these has caused total failures before)

1. **NEVER touch `$HARNESS_STATE/config.toml`** — a previous agent run deleted
   the harness config and everything broke. Only write throwaway configs in
   temp dirs.
2. The harness must run against **fake/test state**: `src/main.rs` refuses to
   start unless BOTH `HARNESS_STATE` and `HARNESS_WORKSPACE` are explicitly
   set. Keep that guard. Never point them at `/state` or the real
   `/workspace` during development.
3. **Zero `unsafe` in `src/`** (plan §2/§8). Tests may use `unsafe` only for
   `std::env::set_var` (Rust 2024 makes it unsafe) guarded by a process-global
   mutex (see `tests/seed.rs::env_lock`).
4. No `spawn_blocking` for I/O; async tokio throughout. No `.expect()`/panic
   on fallible I/O in the agent core — errors are data.
5. Keep `cargo test` green and warning-free at every checkpoint.
6. **Before starting a phase: say which phase you are starting.**
   **After completing a phase: say which phase is done + test results.**
   Update `agent/TODO.md` at the same time.
7. **The repo is public (GitHub) — never write environment-specific or
   sensitive values into `agent/` files or any tracked file**: no real
   backend hostnames or LAN IPs, no credentials/passwords, no personal
   names/emails. Use placeholders (e.g. `llm.example.lan`) and put real
   values only in gitignored files (`run.local.env`, `$HARNESS_STATE`).

## Current status (as of 2026-10-14)

**Phases 1–8: COMPLETE (Phase 8 = model management: load/unload/status,
see notes below); 4 user-reported TUI bugs fixed (2026-10-07):**

1. **Shift+Enter = Enter** — kitty protocol is now DETECTED, not assumed:
   at startup the TUI does the kitty "detect before enabling" handshake
   (`CSI ?1u` + `CSI ?15n`, 100 ms reply window — `kitty_probe` in
   `tui.rs`), BEFORE crossterm's EventStream reads stdin (no competing
   reader race; the probe is placed before seeding, which is the slow part).
   The full flags (disambiguate+alt+report-all = 13, `CSI >13u`) are pushed
   ONLY when the terminal answers; `CSI <1u` still pops on every exit
   path. Shift+Enter (`CSI 13;2u`) works on kitty terminals; the hint line
   advertises `shift+enter` there and `ctrl-j` on legacy terminals.
   `parse_kitty_probe_response` + `find_subseq` are pure and unit-tested.
2. **Git "filesystem boundary" warning leaking over the UI** — new
   `src/logmsg.rs`: all runtime `eprintln!`/`println!` (config, seed,
   gitstore, session, loop) now go through `logmsg::eprintln/writeln`.
   Headless mode prints normally; TUI mode routes them into the
   conversation as `⚠` lines (new `UiEvent::Warn` via an mpsc warn channel)
   instead of spilling raw stderr onto the alternate screen.
3. **No spacing between sections** — `Section` enum + `App::gap_if`:
   blank separator line between DIFFERENT adjacent sections (text /
   reasoning / info / tool / error / note); a tool call and its result are
   the SAME section (attached block), stream continuations extend the open
   stream line (no blank).
4. **Scroll-up jumped to top** — `user_scroll` is now "distance FROM THE
   BOTTOM" as documented; `scroll_offset(user_scroll, total, visible)`
   (pure, unit-tested: bottom-stick, page-up, top clamp, small doc) turns
   it into the top offset; `scroll_up` clamps at the real max (viewport =
   terminal height minus the 1+1+3 pinned rows via `conv_visible_height`).

E2E: `agent/kitty_e2e.py {kitty,legacy}` (pty + minimal CUP screen model;
binary arg, temp state/ws, never touches /state): kitty mode PASSES
(probe reply → `CSI >13u` push, `shift+enter` hint, Shift+Enter +
Ctrl-J insert newlines without submitting, clean /quit); legacy mode
PASSES (no push, `ctrl-j` hint, clean quit — its single-byte input
assertion is skipped as NOTE in THIS sandbox, whose pty line discipline
drops early bytes; verified as a sandbox artifact, not an app bug).

**Test state: 113 tests green (97 lib incl. tui units + 9 backend + 5
compaction + 2 seed), 0 warnings, 0 unsafe in src/, `cargo build
--release` clean.** (rustfmt is not installed in this sandbox —
hand-formatted to match the existing style.)

**2026-10-09 input-box fix (user report "newline not visible on too-long
lines, typos invisible"): long input lines now SOFT-WRAP across multiple
visible rows instead of clamping to one row behind a `…` sliding window —
`render_input_lines` wraps each hard line into `avail`-char rows and
`input_block_lines(input, width)` counts the wrapped rows for the layout
/ scroll-up math (cap still MAX_INPUT_LINES, window keeps the caret's
visible row). The end of an over-long line and any newline after it are
always visible now.**

**2026-10-09 boot black-screen fix (user report "screen is black until
you click a button"): startup hung in the DROP of `prepare_terminal`'s
throwaway current-thread tokio runtime — after `tokio::io::stdin()`
registers the tty, dropping the runtime blocks indefinitely (IO-driver
shutdown stalls; minimal repro: any runtime kind, stdin read, then drop);
the first keystroke completed the shutdown, so the UI "appeared" on the
first click. `kitty_probe` is now synchronous (nix `poll`+`read` on fd 0,
same 100 ms deadline; nix features poll+fs added to the existing dep),
so `prepare_terminal` no longer builds any runtime. Regression:
`agent/first_paint_e2e.py <bindir>` (frame painted with NO input at
~0.23 s + clean /quit). Run the e2e scripts with the binary's dir
(`target/release`), not the repo root.**

**2026-10-09 TUI UX pass (user report): (1) the submitted user request
is now displayed in the conversation (`❯ ` bold green, `  `-indented
continuations, own `Section::User`); (2) the turn header no longer says
"turn 1" forever — `start_turn` is async and reads the number from the
LOOP's live session (`turn_number()+1`, the expression the loop itself
records), since the app's Session snapshot never advanced; (3) section
grammar: `▸ ` block marker on the first line of each agent-text block
(`last_line_is_stream` distinguishes continuation lines from new
blocks), tool result bodies dimmed to DarkGray (agent text is now the
only full-brightness content), bold turn header.**

**2026-10-14 Phase 8 — model management (load/unload/status).** The
`/models` picker is now a model MANAGER: per-model status from the
llama.cpp router fork's `GET /models` (`status.value`: loaded /
loading / unloaded / **sleeping** (= the "hibernating" sleep mode) /
downloading / downloaded; plain OpenAI backends → `[remote]`), colored
tags, `●` current, arrows + enter/number switch, `l` load, `u` unload,
`r` re-scan sources. New client methods `load_model`/`unload_model`
(`POST /models/load|unload`, body `{"model": name}`) + `refresh_models`
(`GET /models?reload=1`) — NOTE: those MANAGEMENT routes live ONLY at
the server ROOT, not under `/v1` (verified live on llm.example.lan);
`BackendClient::admin_base()` strips a trailing `/v1`|`/api`. Ops run
as background tasks that re-poll `/models` 1/s (modal live-updates via
`UiEvent::ModelsRefresh`) until settled (120 s cap); the op survives
closing the modal (`App::model_op`) and the result lands as a modal
note or conversation line (`UiEvent::ModelOpDone`); one op at a time.
New commands `/load <name>`, `/unload [name]` (default = current).
`ModelStatus`/`ModelInfo` + lenient parse in `backend/client.rs`.
E2E caught a pre-existing Phase-6 bug while testing: `draw_modal`
painted `Block::bordered()` LAST, overpainting the title+footer rows
(the approval modal's y/a/n hint was invisible) — border now renders
first, content in the inner rect. E2E: `agent/models_e2e.py <bindir>`
(pty, temp state/ws + throwaway config.toml → LAN backend, READ-ONLY:
never presses l/u against the shared backend). 113 tests green
(97 lib + 9 backend + 5 compaction + 2 seed), 0 warnings, 0 unsafe.**

**2026-10-09 refactor (user-requested): token counters come from the
STREAM, not internal counting.** The llama.cpp server streams live
per-chunk counters (`timings`: `predicted_n` / `prompt_n`+`cache_n`)
when the request sets `timings_per_token: true` + `return_progress: true`
(the fields the production payload
`reference/llama_cpp_completion_with_thinking_example` sends; verified in
the fork source and live against llm.example.lan). Now: stream payload
sends both; `SseParser` emits `SseEvent::Usage` per chunk (from `usage`
or `timings` objects; last write wins into `Done.usage`);
`AgentLoop::stream()` takes the turn counters by `&mut`, folds live
updates in (prompt = max, completion = base + per-request running max)
and emits `usage` UI events mid-stream (deduped); the chars/4
`completion_chars` estimate in text/reasoning events is gone —
`estimate_prompt_tokens` remains only as no-reporting fallback + (estimating
only the unsent tail) budget gate; the TRIGGER itself is driven by the live
server count, and the threshold is `ContextConfig::threshold_tokens()` in
both the gate and `ensure_budget` (2026-10-09 follow-up). Verified live: real counts in turn_end (1138/27), live prefill
progress in the raw stream.

### Phase details

**Phases 1–6 (64 tests green at that checkpoint, 0 warnings, 0 unsafe):**
- Phase 6: `src/tui.rs` — ratatui + crossterm TUI (thin renderer; the
  loop is the brain). Seams: emit -> mpsc `UiEvent`, ask_approval -> modal +
  oneshot (turn task blocks, UI stays responsive — the worker-thread
  analogue). `Arc<tokio::sync::Mutex<AgentLoop>>`; cancel via cloned watch
  sender (lock-free, like Python's `Event.set`). Conversation (stick-to-bottom
  Paragraph, PageUp/Down), status bar, approval modal (y/a/n, Esc denies),
  model modal + `/models` `/model` (persists). Commands `/help /models /model
  /new /compact /prompt /quit`; Ctrl-C/q interrupts or quits. Verified live
  in a pty (renders, /help, /prompt, /new, clean quit + terminal restore).
  Also fixed `tools/registry::execute` to scope the registry lookup before the
  await (a std `MutexGuard` must not cross an await, or spawned turn tasks
  become non-Send).
- Phase 5: `AgentLoop::update_system_prompt(text)` (plan §4.2) — swaps
  `memory[0]` + records a `prompt` event via `Session::record_prompt`
  (replay side was already in `session.rs`). Unit tests in
  `loop_.rs::tests`: memory[0] injection + event recorded + replay
  consistency, override survives resume from disk, old logs without
  `prompt` events replay unchanged. (Agent-folder prompt injection +
  tests were done early in Phase 4 in `prompts.rs`.)
- Phase 1: `config.rs`, `state/{seed,gitstore}.rs`
- Phase 2: `tools/{registry,files,shell,web}.rs` — shell uses
  `tokio::process::Command` + `.process_group(0)` + `tokio::time::timeout` +
  safe `nix::killpg` (do not regress to std::process/raw pre_exec)
- Phase 3: `backend/client.rs` (async SSE, retry), `agent/session.rs`
  (JSONL + replay; cross-language parity with Python verified)
- Phase 4: `agent/compaction.rs` (faithful port; Python's monkeypatchable
  `_find_cut` global became the injectable `FindCut` trait —
  `DefaultFindCut`/`NoopFindCut`), `agent/loop_.rs` (async `run_turn`;
  cancel = tokio watch-bool mirroring Python's `threading.Event`, NOT a
  CancellationToken), `main.rs` headless one-turn run (bare `harness` ->
  ready message; exit 1 on `!ok`; stdin approval, EOF denies),
  `tests/compaction.rs` (all 5 reference tests ported, end-to-end
  mid-chain compaction incl. real shell), smoke run vs
  `https://llm.example.lan/v1` (gemma-4-31B) passed.

**Phase 7: DONE — code complete; live create→compact→resume verified
(qwen_3_8_27B). One item left: podman build/run, blocked only by this
sandbox's podman (empty subuid/subgid). See "Phase 7 status" below.**

## Phase 7 status (in progress)

Done (code in tree):
- `Containerfile` (Fedora 44, builds the release binary, `ENTRYPOINT
  ["harness"]`, git identity), `build.sh`, `run.sh` (TUI / `once` / `resume`
  modes; mounts `/workspace` + `/state`, passes `-e HARNESS_STATE=/state
  -e HARNESS_WORKSPACE=/workspace -e HARNESS_TLS_VERIFY=0`, seeds a
  config.toml on first run, `--add-host` for the LAN backend).
- `harness resume "prompt"` headless path (reopens the most recent session,
  replays it summary-aware, runs a turn) — this is what makes create →
  compact → resume reachable in the shipped binary (the Python reference
  only ever creates fresh sessions; resume lives in the TUI).
- `harness once "prompt"` accepted as an alias for the bare one-shot form
  (plan §5-Q4 names `harness [tui]` / `harness once "prompt"`).
- `README.md` (adapted from the reference to the Rust implementation).
- E2E create → resume verified live earlier: one session, `harness` then
  repeated `harness resume` appended turns to the SAME session and created
  files across separate process invocations.

Still to verify (blocked by environment, not by the code):
1. ~~Live "create → COMPACT → resume"~~ — DONE (qwen_3_8_27B, synthetic
   budget in a throwaway config): summary events + kept-tail `dup` events
   in the right order, resume replays the summary, over-small-budget abort
   paths fire as designed.
2. `podman build` + `podman run` end-to-end — this sandbox's podman is
   rootless with EMPTY /etc/subuid + /etc/subgid, so pulling the Fedora
   base image fails during unpack (environment limitation, confirmed today;
   the Containerfile mirrors the proven reference structure + a standard
   `cargo build --release`). On a podman host: `./build.sh` + `./run.sh`.

## Later phases (plan §7)

- ~~**Phase 6 — TUI**~~ — done (see above).
- **Phase 7 — container + polish**: see "Phase 7 status" above — only the
  podman build/run verification remains, blocked by this sandbox's podman.
- ~~**Phase 8 — model management**~~ — done (2026-10-14, user request):
  load/unload/status per model, see the Phase 8 notes in "Current status"
  above. New: `ModelStatus`/`ModelInfo` + `load_model`/`unload_model`/
  `refresh_models`/`admin_base` in `backend/client.rs`; TUI model manager
  modal (l/u/r keys) + `/load` + `/unload` commands; `draw_modal` border
  fix (was overpainting title+footer on BOTH modals). E2E
  `agent/models_e2e.py <bindir>` (read-only vs the shared backend).

## Verify state (run these at the start of a session)

```sh
cargo test 2>&1 | grep -E "test result|FAILED"   # expect all "ok", 0 failed
grep -rn "unsafe" src/ | grep -v "//"             # expect only comments
grep -rn "unimplemented!" src/                    # expect none
```

## Open questions

(All Phase 4 open questions resolved: bare `harness` prints the ready
message; `harness "prompt"` runs one turn. `FindCut` injection seam replaces
Python's monkeypatching. Cancel is a watch-bool, not a CancellationToken.)
