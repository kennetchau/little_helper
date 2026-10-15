# Plan: Rewrite the Python harness in Rust

Target: port `reference/harness/` into this crate (`harness_rust`, `src/`),
preserving behavior (JSONL session format, replay contract, compaction
invariants), and adding two new features: **SKILL.md injection** and a
**system-prompt update function**.

## 1. What the Python harness does (module map)

| Python module | Lines | Role | Rust port |
| --- | --- | --- | --- |
| `config.py` | 192 | TOML config over defaults, env overrides, lenient validation | `config.rs` — `serde` + `toml` |
| `state/seed.py` | 24 | First-run seed of state dir + git repos (idempotent) | `state::seed` |
| `state/gitstore.py` | 47 | `run_git`, `ensure_repo`, `commit_all` (never raise) | `state::gitstore` |
| `backend/client.py` | 231 | OpenAI-compatible client: SSE streaming, one-shot `complete`, `GET /models`, 1 retry on 5xx, `BackendError` only error type | `backend::client` — `reqwest` |
| `agent/session.py` | 228 | Append-only JSONL events; `replay()` = system + all summaries + msg/tool events after last summary | `agent::session` |
| `agent/compaction.py` | 248 | `_find_cut`, `_find_round_cut`, `compact_once`, `ensure_budget`; summaries never re-summarized; kept tail re-recorded with `dup: true` | `agent::compaction` |
| `agent/loop.py` | 518 | Turn state machine: budget check → stream → tool calls (approval → exec) → mid-turn budget → wrap-up (diff + auto-commits); `cancel()` is cooperative | `agent::loop` + `agent::prompts` |
| `tools/registry.py` | 118 | Registry, `ToolResult{result, ok}`, path jail, output cap, schemas | `tools::registry` |
| `tools/files.py` | 250 | list_dir, read_file, write_file, edit_file, delete_file, grep, glob, git_diff | `tools::files` |
| `tools/shell.py` | 165 | run_command: new session (pgrp), killpg on timeout, `unshare -n` when `exec_network=false`, PID-1 zombie reaper | `tools::shell` |
| `tools/web.py` | 87 | web_search (ddgs), web_fetch (httpx + trafilatura) | `tools::web` |
| `tui/app.py` | 624 | Textual TUI: conversation widget, status bar, approval/model modals, worker threads | `tui/` — **ratatui + crossterm** (see §5) |
| `tests/test_compaction.py` | 213 | Replay-consistency + mid-chain compaction regression tests | `tests/compaction.rs` |

## 2. Dependency mapping

| Python | Rust crate | Notes |
| --- | --- | --- |
| httpx | `reqwest` (async) | `rustls-tls` + `stream` (SSE line splits) |
| — (new) | `tokio` | multithreaded runtime; `rt-multi-thread`, `macros`, `process`, `sync`, `time` |
| trafilatura | **`trafilatura`** (crates.io, confirmed) | `extract(html, &Options::default())` → `content_text`; close 1:1 |
| ddgs (reimpl) | `scraper` + `selectors` | Parse DDG HTML results into the same `{title, url, body}` shape |
| ddgs | **none — reimplement** | No Rust port. Replace with `html.duckduckgo.com/html/` endpoint
   (GET + parse `result__a`/`result__snippet` with `scraper`+`selectors`),
   same shape as ddgs's HTML backend. Verified reachable from this container
   with a browser-like User-Agent (10 results per page). |
| tomllib | `toml` + `serde` | |
| rich (headless) | plain ANSI via `termcolor` (or raw `eprint!`) | keep it minimal |
| textual | `ratatui` + `crossterm` | only if we port the TUI (see §5-Q3) |
| difflib (edit preview) | `similar` (`TextDiff::unified_diff`) | |
| re (edit preview, persist) | `regex` | |
| json | `serde_json` | `Value` mirrors Python's `dict`-as-`dict` for wire messages |
| std (subprocess, signals, /proc reaper) | `std::process` + `nix` (killpg, setsid) | `unshare -n` stays a subprocess call; PID-1 reaper stays /proc-based |

Concurrency decision: **tokio + async, by design (not a 1:1 port of the
thread model).** You asked not to replicate the Python flow where a modern
design is better. Shape:
- single `#[tokio::main]` multithreaded runtime; `run_turn` is an
  `async fn`; the backend client is async `reqwest` with an `async Stream`
  over the SSE body (no blocking client, no `spawn_blocking` for I/O).
- **cancel** = `tokio_util::sync::CancellationToken` (or a `watch` bool)
  checked at the same safe points as Python's `threading.Event`, plus
  `tokio::select!` inside the stream loop so an in-flight stream is
  aborted promptly.
- **approval** = async: the loop `await`s a `tokio::sync::oneshot` —
  headless mode feeds it from a stdin reader task; TUI feeds it from the
  modal. No blocked threads at all.
- **run_command** = `tokio::process::Command` with a new process group;
  timeout via `tokio::time::timeout`; kill the group on expiry. The PID-1
  /proc reaper stays a plain std daemon thread (harmless, only armed when
  `getpid() == 1`; normally `podman --init` covers it).
- **No `unsafe` anywhere.** Errors as data for the model (as in Python),
  `thiserror` for the one abort-type error (`BackendError`), `anyhow` in
  `main`. No raw pointers, no `unwrap` on fallible user/env paths in the
  agent core — the "this module never raises" discipline from Python carries
  over as "never `.expect()` on I/O".

## 3. Wire/message model

Messages stay `Vec<serde_json::Value>` in OpenAI wire format — exactly like
Python's `list[dict]`. No custom message type; this keeps replay, compaction
and backend payload construction trivial to verify against the reference.
Session events likewise `serde_json::Value`. This also guarantees the Rust
harness can **resume session files written by the Python one** (and vice
versa) — the JSONL schema is unchanged.

## 4. New features

### 4.1 Agent folder → system prompt subsection (final spec)

- **Composition** in `agent::prompts`: `system_prompt = base_prompt(agent)
  + agent_folder_section()`.
- **Discovery rule** (probing, at session start / replay time):
  1. Look for an **agent folder** at the top level of the workspace,
     matched **case-insensitively** (`agent/`, `Agent/`, `AGENT/`).
  2. If it exists, load **only the two stable cross-session files**
     directly inside it (non-recursive), matched case-insensitively by
     name: **`AGENT.md`** (session continuity / ground rules) and
     **`SKILL.md`** (workspace-specific instructions). The folder is
     ALSO the agent's working space (scratch files, progress notes) —
     anything else (TODO.md, temp files, ...) is **never injected**:
     volatile state stays out of the fixed system prompt and is read on
     demand.
  3. Each file is text only (NUL bytes in the first 8 KB -> skipped);
     each file capped at 20,000 chars, section total capped at 50,000
     chars. If any file is loadable, append to the system prompt:
     ```
     ## Agent notes

     The workspace contains an agent folder. Follow its contents:

     ### <filename>
     <content>
     ```
- Detection runs **at session start / replay time** (the prompt is
  memory[0]); the effective prompt is recorded in the session log (see
  4.2) so a resume reconstructs exactly what was sent. If the folder
  changes mid-session, the next session (or `update_system_prompt`)
  picks it up.
- Note: this repo's own `agent/` scratch folder is picked up by this rule
  when the workspace is this repo; the progress todo lives there.

### 4.2 `update_system_prompt(text)` function

- `AgentLoop::update_system_prompt(&mut self, text: &str)`:
  - replaces `memory[0]` with the new system message;
  - appends a **new event type** `{"type": "prompt", "text": ...}` to the
    session JSONL;
  - `Session::replay()` is extended: system message = content of the **last**
    `prompt` event, falling back to the composed default (base + skills)
    when none exists.
  - Old sessions without `prompt` events replay exactly as before
    (backward compatible with Python-written logs).
- Exposed as:
  - headless: a `/prompt <text>`-style mechanism is TUI-only; headless can
    call it programmatically (library API);
  - TUI command `/prompt` when the TUI lands.
- This is the cleanest spot for it: the Python `replay()` already special-
  cases `agent` events for name changes — `prompt` follows the same pattern.

## 5. Open questions — ALL ANSWERED

- Q1 web search: **Yes** — DuckDuckGo HTML reimplementation.
- Q2 agent notes: final spec — **probe for an `agent` folder
  (case-insensitive); inject only `AGENT.md` + `SKILL.md` from it**
  (§4.1). The rest of the folder is the agent's scratch workspace and is
  never injected.
- Q3 TUI: **important** — stays in this project's scope (still built after
  the headless core so each layer is testable, but it is a deliverable,
  not a stretch goal).
- Q4 container: answered via reference files (§5-Q4 text below); backend
  path confirmed as **`.../v1` chat completions** → default backend for this
  env is `https://llm.example.lan/v1`.

1. **Web search engine** — `ddgs` has no Rust equivalent. **Resolved by
   container testing**: the Fedora 44 container has general internet access
   (duckduckgo.com reachable; `llm.example.lan` is the LLM backend, not a
   search proxy). Proposed: reimplement the `ddgs` HTML backend —
   `GET https://html.duckduckgo.com/html/?q=…` with a browser-like
   User-Agent, parsing `result__a` (title/URL) + `result__snippet`
   (verified: 10 results per page; note: 403 without a real UA header).
   No new config key needed beyond existing `max_results`. **Confirm
   OK?** (SearXNG JSON remains an easy swap later if you self-host one.)
2. **SKILL.md location(s)** — **RESOLVED (2026-10-04)**: inject only
   `agent/AGENT.md` + `agent/SKILL.md` (case-insensitive) from the
   workspace agent folder — the folder doubles as the agent's scratch
   workspace, so everything else stays out of the system prompt (see
   §4.1).
3. **TUI scope** — the Textual app is ~1/4 of the codebase. Proposal:
   **phase it** — ship headless first (config summary + one-shot turn, like
   `python -m harness "..."`), then port the TUI to ratatui with the same
   commands (`/help /models /model /new /compact /quit`) + the new `/prompt`.
   OK, or do you want TUI in the critical path?
4. **run.sh / container story — answered from `reference/`**: base image is
   Fedora 44 with `git, python3, uv, nodejs, rust, cargo, gcc/clang, make,
   cmake, ps, podman`; workspace mounted at `/workspace`, state at `/state`;
   run via podman `--init` (so the PID-1 reaper is NOT needed in the normal
   path — keep it anyway for direct runs), `HARNESS_TLS_VERIFY=0`, and
   `llm.example.lan` (192.168.x.x) is the LLM backend host (backend is at
   443, so the default `base_url` for this env will be
   `https://llm.example.lan/...` — confirm exact path). I'll adapt
   `Containerfile`/`build.sh`/`run.sh` to build the Rust binary in the image
   and `ENTRYPOINT ["harness"]` with subcommands `harness [tui]` /
   `harness once "prompt"`. **Confirmed in scope.**
   Note: the Rust binary refuses to start unless **both** `HARNESS_STATE`
   and `HARNESS_WORKSPACE` are set explicitly (dev-safety: a bare `cargo run`
   must never touch the real `/state` or `/workspace`), so the adapted
   `run.sh` passes `-e HARNESS_STATE=/state -e HARNESS_WORKSPACE=/workspace`.

## 6. Target layout

```
src/
├── main.rs              # headless entry: seed, config panel, optional one turn
├── config.rs            # Config data structs, defaults, load/seed template
├── agent/
│   ├── mod.rs
│   ├── prompts.rs       # base prompts, agent-folder/agent.md probe, system prompt composition
│   ├── session.rs       # JSONL store + replay (incl. new `prompt` events)
│   ├── compaction.rs    # cuts, compact_once, ensure_budget
│   └── loop.rs          # AgentLoop, run_turn, approval seam, emit seam
├── backend/client.rs    # reqwest SSE client, complete(), list_models()
├── state/
│   ├── mod.rs
│   ├── seed.rs
│   └── gitstore.rs
├── tools/
│   ├── mod.rs
│   ├── registry.rs      # ToolContext, jail, cap, dispatch
│   ├── files.rs
│   ├── shell.rs
│   └── web.rs
└── tui/                 # (phase 5, if confirmed) ratatui app
tests/
└── compaction.rs        # port of tests/test_compaction.py (FakeClient trait)
```

## 7. Build order (each phase compiles + tested before the next)

1. **Scaffold**: Cargo deps, `config.rs`, `state/{seed,gitstore}.rs` —
   verify: seed an ephemeral state dir, config round-trips.
2. **Tools**: `registry` + `files` + `shell` (+ `web` once Q1 answered) —
   unit tests: jail, edit_file uniqueness, glob/grep, exec timeout/killpg.
3. **Backend + session**: `backend::client` (async SSE parser, retry,
   usage), `agent::session` (replay contract) — tests against a fake local
   HTTP server (`wiremock`).
4. **Compaction + loop + headless main**: port `compaction.py` and
   `loop.py` (async); port all 5 tests from `test_compaction.py`
   (FakeClient → async trait `Backend { stream_chat, complete }`); smoke
   run `harness "create hello.txt containing: hi"` against
   `https://llm.example.lan/v1` (`HARNESS_TLS_VERIFY=0`).
5. **New features**: `agent::prompts` (agent-folder/agent.md probe) +
   `update_system_prompt` + `prompt` event in replay; unit tests: agent
   notes injection, prompt override survives resume, old logs replay
   unchanged.
6. **TUI** (a deliverable — Q3): ratatui port — conversation view, status
   bar, approval modal, model modal, commands incl. `/prompt`.
7. **Container + polish**: adapt `Containerfile`/`build.sh`/`run.sh`
   (build the binary in the image, `ENTRYPOINT ["harness"]`), README,
   final end-to-end session check (create → compact → resume).

## 8. Risks / fidelity notes

- **SSE parsing** must match Python: `data:` lines, `[DONE]`, usage-only
  chunks, `reasoning_content` deltas, tool-call deltas accumulated by index.
  Covered by phase-3 fake-server tests.
- **Compaction invariants** (replay consistency, group integrity, strict
  shrink, mid-turn round cuts) are the heart of the system — the ported
  test suite is the acceptance gate; do not "simplify" the cut logic.
- **`unshare -n` + PID-1 reaper**: keep both behaviors; test the reaper
  only if the test env runs as PID 1.
- **Edit preview diff** (`similar` unified output) is cosmetic — exact
  match with difflib is not required, presence/shape is.
- **Safety rule (per your request): zero `unsafe`.** `clippy` with
  `-D warnings` in CI-style local checks; process killing uses `nix`
  (killpg/setsid) rather than hand-rolled syscalls; the reaper reads
  `/proc` only (no `fork()` in Rust).
- Minor Python quirk noticed: `__main__.py` TUI branch is unreachable
  (dead code after an early `return`). Not ported; TUI gets its own
  subcommand (`harness tui`).
