# little_helper

An AI agent harness (binary: `little_helper`) for code creation and project updates, in Rust. A headless
agent loop drives an OpenAI-compatible backend to work on a git-tracked
workspace, with per-tool approval, append-only JSONL session logs, automatic
context compaction, and a ratatui/crossterm TUI on top.

This is a faithful port of the Python harness in `reference/harness/`; the
plan is `plan.md`.

## Requirements

- A recent Rust toolchain (`rust` + `cargo`, see `Containerfile`).
- An OpenAI-compatible chat backend (e.g. `https://llm.example.lan/v1`).

## Setup

```sh
cargo build --release
```

On first run, `harness` seeds its state directory (`$HARNESS_STATE`, default
`/state`) with a default `config.toml` and initializes git repos for both the
state directory and the workspace (`$HARNESS_WORKSPACE`, default
`/workspace`). Seeding is idempotent.

The binary **refuses to start** unless both `HARNESS_STATE` and
`HARNESS_WORKSPACE` are set explicitly, so a bare `cargo run` never touches a
real global state/workspace.

## Usage

**In a container (recommended)** — `run.sh` builds/runs the harness in podman (image `little_helper`) with the
with the project and a persistent state dir mounted:

```sh
./build.sh                              # build the image once
./run.sh /path/to/project               # TUI
./run.sh once "prompt" /path/to/project # headless one-shot
./run.sh resume "prompt" /path/to/project # continue the latest session
```

`run.sh` seeds a `config.toml` into the host state dir on first run (backend
+ model), and passes `-e HARNESS_STATE=/state -e HARNESS_WORKSPACE=/workspace`
plus `HARNESS_TLS_VERIFY=0`. Env knobs: `IMAGE` (default `little_helper`),
`STATE_DIR` (default `$HOME/little-helper-state`), `CPUS` (8), `MEMORY` (16g),
`BACKEND` and `MODEL`. Host-specific values (your backend URL, model name,
Postgres password) belong in an untracked **`run.local.env`** next to
`run.sh` (gitignored) — e.g. `BACKEND=https://your-llm-host/v1` and
`MODEL=your-model` — so nothing environment-specific is committed. A
random Postgres password for the local companion container is generated
once and kept in `$STATE_DIR/.pg_password` (override with `PG_PASSWORD`).

**Directly** (headless):

```sh
HARNESS_STATE=/tmp/harness-state HARNESS_WORKSPACE=/tmp/ws \
  target/release/little_helper "create hello.txt containing: hi"
```

`little_helper` (no prompt) seeds and prints the config summary; `little_helper tui`
launches the TUI; `little_helper resume "prompt"` reopens the most recent session
(replaying it, summary-aware) and runs one more turn in it — the create →
compact → resume path.

TUI commands (typed in the prompt input):

| Command | Effect |
| --- | --- |
| `/help` | Show available commands |
| `/models` | Model manager: lists backend models with their status (`loaded` / `loading` / `sleeping` / `unloaded`, or `remote` when the backend reports no status). In the modal: `↑↓` move, `enter` (or a number) switches, `l` loads, `u` unloads, `r` re-scans the model sources |
| `/model <name>` | Switch model (persisted to `config.toml`) |
| `/load <name>` | Load a model into the backend (`POST /models/load`) |
| `/unload [name]` | Unload a model, default the current one (`POST /models/unload`) |
| `/new` | Start a new session |
| `/compact` | Compact the context now |
| `/prompt <text>` | Set the system prompt (survives resume) |
| `/quit` | Exit |

`Ctrl-C` stops a running turn only (the loop stops at the next safe point and
discards partial output) — it never quits the app; use `/quit` to exit. In the TUI the
approval modal answers with `y`/`a`/`n`; `Esc` denies.

## Configuration

Config is loaded from `$HARNESS_STATE/config.toml` over built-in defaults.
Unknown keys are ignored; invalid enum values fall back to the default with a
stderr warning. The default file is written on first run and is git-tracked.

| Section | Keys | Notes |
| --- | --- | --- |
| `[backend]` | `base_url`, `api_key`, `model`, `temperature`, `max_tokens`, `thinking_budget` | OpenAI-compatible endpoint |
| `[context]` | `max_tokens`, `summarize`, `summarize_threshold`, `keep_recent_turns`, `summary_max_tokens` | `summarize` = `off` \| `auto` \| `manual`; auto compaction triggers when prompt tokens exceed `max_tokens * summarize_threshold` |
| `[workspace]` | `path`, `auto_git` | Workspace root; auto-commit workspace changes each turn |
| `[tools]` | `enabled` | Master switch for all tools |
| `[tools.approval]` | per-tool level | `off` (hidden) \| `ask` (prompt) \| `auto` (run freely) |
| `[tools.limits]` | `max_read_lines`, `exec_timeout_sec`, `exec_output_chars`, `web_max_chars`, `exec_network`, `max_tool_rounds` | `exec_network = false` runs commands in a fresh network namespace via `unshare -n` |
| `[search]` | `engine`, `max_results` | Web search settings |

Environment variables:

| Variable | Effect |
| --- | --- |
| `HARNESS_STATE` | State directory (default `/state`) |
| `HARNESS_WORKSPACE` | Workspace path (default `/workspace`) |
| `HARNESS_TLS_VERIFY` | Set to `0`/`false`/`no`/`off` to disable TLS verification |

## How it works

**Agent loop** (`src/agent/loop_.rs`) — one `run_turn(user_message)` call
drives a full turn: budget check (compaction) → streaming request →
(tool calls → approval → execute → tool results → budget check)\* → final
message → wrap-up (final diff + auto-commits). Memory is a plain list of
OpenAI wire-format messages kept in lockstep with the session log, so a
resumed session replays to exactly the same memory. Cancellation is a
`tokio::sync::watch` bool mirroring Python's `threading.Event`; `run_turn` is
`async` and the loop is shared as `Arc<tokio::sync::Mutex<AgentLoop>>` so the
TUI's turn runs in a task while the UI stays responsive.

**Agent notes / system prompt** (`src/agent/prompts.rs`) — the system
prompt is composed at session start as base prompt + an `## Agent notes`
subsection when the workspace has a top-level **agent folder**
(case-insensitive `agent/`/`Agent/`/`AGENT/`). The folder is the agent's
working space, so only two files are injected from it (case-insensitive,
text only, 20k chars each, 50k total): `AGENT.md` (session continuity /
ground rules) and `SKILL.md` (workspace-specific instructions). Scratch
files (TODOs, temps) are never injected — volatile state is read on demand,
not baked into the fixed system prompt. `/prompt` (or the library API) can
override the whole prompt; the override is recorded and survives resume.

**Sessions** (`src/agent/session.rs`) — append-only JSONL event logs under
`$HARNESS_STATE/sessions/<id>.jsonl` (id: `YYYYMMDD-HHMM-<agent>`). Events
are the audit log; `replay()` rebuilds working memory as: system message + all
summaries in order + all message/tool events after the last summary. A `prompt`
event (from `AgentLoop::update_system_prompt`) overrides the system message and
survives resume.

**Compaction** (`src/agent/compaction.rs`) — when the context budget is
exceeded, the oldest real turns (keeping the most recent `keep_recent_turns`)
are summarized into a single summary message. Cuts land on turn boundaries so
assistant/tool-call groups are never split, and existing summaries are never
re-summarized. The budget is checked at turn start **and after every tool
round**, so a long tool-call chain compacts as it grows. Python's
monkeypatchable `_find_cut` global is the injectable `FindCut` trait
(`DefaultFindCut` / `NoopFindCut`) — a documented deviation.

**Auto-commits** (`src/state/gitstore.rs`) — with `auto_git`, each turn commits
workspace changes (message: first line of the final reply) and state changes
(`session <id>: turn <n>`). Git failures warn but never kill a turn.

## Design notes

- **Zero `unsafe` in `src/`** (plan §2/§8). Process killing uses `nix`
  (`killpg`/setsid); shell tools run in their own process group
  (`.process_group(0)`) with a `tokio::time::timeout` that kills the whole
  group on expiry. Tests use `unsafe` only for `std::env::set_var`, guarded by
  a process-global mutex (`tests/seed.rs::env_lock`).
- **Async throughout** (tokio). No `spawn_blocking` for I/O; the backend client
  is an async `reqwest` streaming SSE. Errors are data for the model; only
  backend/loop failures abort a turn.

## Tests

```sh
cargo test
```

Covers config, tools (path jail, edit uniqueness, shell timeout/group-kill),
the SSE contract (wiremock), cross-language replay parity, and all five
reference compaction tests (replay consistency, group integrity, strict
shrink, mid-turn round cuts) ported to the async `Backend` seam.
