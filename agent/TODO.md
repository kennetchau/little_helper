# Progress — Rust harness rewrite

      (2026-10-14, user request "more sophisticated approach: load and
      unload model in the harness; /models should show what model is
      available, loaded, unloaded or hibernating"):
      - [x] SERVER CONTRACT from `reference/llama.cpp` (the user's clone
            of the fork, `tools/server/server-models.{h,cpp}`):
            `GET /models` entries carry `status.value` ∈ loaded /
            loading / unloaded / sleeping / downloading / downloaded
            ("hibernating" = llama.cpp **sleep** mode, e.g.
            `--sleep-idle-seconds`), plus `failed`+`exit_code`, `source`
            (preset/models_dir/cache), `context_length`;
            `POST /models/load` + `POST /models/unload` take
            `{"model": name}`; `GET /models?reload=1` re-scans sources.
            CRITICAL (verified live on llm.example.lan): the MANAGEMENT
            routes exist ONLY at the server ROOT — `/v1/models/load`
            404s ("File Not Found" from the HTTP router), `/models/load`
            works. Plain `GET /v1/models` works on both.
      - [x] `client.rs`: `ModelStatus` (from_wire/is_transitional/
            is_serving/is_running/label) + `ModelInfo` (lenient
            `model_info()` parse — plain OpenAI entries → `Unknown`/
            `[remote]`); `list_models() -> Vec<ModelInfo>`; `refresh_
            models()` (?reload=1 at the admin root); `load_model` /
            `unload_model` (reuse the 5xx-retry/4xx-fast-fail `send()`);
            `admin_base()` strips a trailing `/v1`|`/api` segment for the
            management routes (pure + unit-tested, incl. nested prefix,
            trailing slash, `v1x` not stripped).
      - [x] TUI model MANAGER (was a dumb numbered picker): per-model
            status tag colored (loaded green / loading+downloading
            yellow / sleeping cyan / downloaded blue / unloaded dim /
            failed red / remote gray), `●` current marker, `>` selection
            (arrows wrap), footer key hints. Keys: enter/number = switch
            (persisted, as before), `l` = load, `u` = unload, `r` =
            re-scan sources, Esc = close. Ops run in a background task:
            POST, then 1s `/models` re-polls (modal live-updates via
            `ModelsRefresh`) until the target settles or 120 s cap; the
            op survives closing the modal (`App::model_op`) and the
            final `ModelOpDone` lands as a modal note or a conversation
            line; one op at a time.
      - [x] Commands: `/load <name>`, `/unload [name]` (default =
            current model); `/help` + module docs updated; README command
            table updated.
      - [x] BUG FOUND BY E2E (pre-existing since Phase 6, both modals):
            `draw_modal` rendered `Block::bordered()` LAST, overpainting
            the title and footer rows (the approval modal's y/a/n hint
            was invisible all along). Border is now rendered first and
            the title/body/footer laid out in the inner rect.
      - [x] Tests: +8 lib (status parse + predicates, fork/OpenAI/failed
            JSON shapes, admin_base, model-modal lines, Models/
            ModelsRefresh/ModelOpDone event handling) + 3 wiremock
            (fork-status list parse, load/unload POST contract, /v1→root
            path stripping, 400 surfaced with server message).
      - [x] VERIFIED LIVE: `/v1/models` on llm.example.lan shows the fork
            contract (gemma-4-31B `sleeping`, qwen_3_8_27B `loaded`,
            qwen_3_6_35B_Heretic_MTP `unloaded`); root-only management
            routes confirmed (400 "already running" / 404); E2E
            `agent/models_e2e.py <bindir>` (pty, temp state/ws with a
            throwaway config.toml pointing at the LAN backend, READ-ONLY
            — never presses l/u against the shared backend): ALL PASS
            (status tags, hint line, r refresh, clean quit). first_paint
            + kitty + legacy E2Es still PASS. 113 tests green
            (97 lib + 9 backend + 5 compaction + 2 seed), 0 warnings,
            0 unsafe in src/, release build clean.

Track of the `plan.md` phases. Update as you go.

**Protocol (per user): announce which phase you are starting before work,
and which phase is done (+ test results) when it completes. AGENT.md at the
repo root has the full handoff notes — read it first.**

- [x] Plan (plan.md)
- [x] **Phase 1 — scaffold**: Cargo deps, `config.rs`, `state/{seed,gitstore}.rs`
      - verify: seed an ephemeral state dir, config round-trips
- [x] **Phase 2 — tools**: registry + files + shell + web
      - unit tests: jail, edit_file uniqueness, glob/grep, exec timeout/killpg
      - web_search verified live (DDG HTML backend); unshare -n refused-not-relaxed
      - dev-safety guard in main: refuses to run without explicit HARNESS_STATE+HARNESS_WORKSPACE
      - shell rewritten to tokio::process + process_group(0) + time::timeout
        + nix::killpg (zero-unsafe compliant); group-kill test passes
- [x] **Phase 3 — backend + session**: async SSE client, session replay
      - tests vs fake HTTP server (wiremock): SSE contract, complete, list_models,
        5xx-retry/4xx-fast-fail
      - cross-language replay parity verified against the real Python module
        (Python-written JSONL -> Rust replay == Python replay, and vice versa)
- [x] **Phase 4 — compaction + loop + headless main**: COMPLETE
      - [x] `Backend` trait added to `backend/client.rs` (dyn-safe, boxed
        futures) — the fake-client seam for the ported tests
      - [x] `agent/prompts.rs` (base prompt + agent-folder probe, §4.1) with
        unit tests — Phase 5 material, written early
      - [x] `agent/{compaction,loop_}.rs` created as marked stubs
        (`unimplemented!`) so the tree compiles
      - [x] port `compaction.py` -> `agent/compaction.rs` (async; faithful —
        do NOT simplify the cut logic, plan §8). Python's monkeypatchable
        `_find_cut` global became the `FindCut` trait seam
        (`DefaultFindCut` / `NoopFindCut`) — documented deviation.
      - [x] port `loop.py` -> `agent/loop_.rs` (async; `tokio::select!` +
        watch-bool cancel mirroring Python's `threading.Event`
        set/clear/is_set; injectable ask/emit callbacks; preview with
        `similar`). Cancelled streams are dropped = connection closed, like
        Python's `resp.close()`.
      - [x] wire real headless run in `main.rs` (bare `harness` prints the
        ready message; `harness "prompt"` runs one turn, exit 1 on !ok;
        stdin approval, EOF denies)
      - [x] port all 5 tests from `tests/test_compaction.py` ->
        `tests/compaction.rs` (FakeClient/ChainClient -> `impl Backend`);
        end-to-end `test_loop_checks_budget_mid_chain` runs real shell
        commands + in-chain compaction
      - [x] smoke: `harness "create hello.txt containing: hi"` vs
        https://llm.example.lan/v1 (gemma-4-31B, HARNESS_TLS_VERIFY=0, temp
        state/ws) — full turn: stream -> write_file -> diff ->
        auto-commits -> turn_end, exit 0. (Default-approval run also
        verified: EOF denies, turn continues as data for the model.)
- [x] **Phase 5 — new features**: `update_system_prompt` + `prompt` event
      replay (replay side already done + tested in session.rs; agent-folder
      prompt injection already wired in `AgentLoop::new`)
      - [x] `AgentLoop::update_system_prompt(text)` (loop_.rs): swaps
        memory[0] + `Session::record_prompt` -> `{"type":"prompt","text"}`
      - [x] tests: memory[0] injection + prompt event recorded + replay
        consistency; override survives resume (fresh AgentLoop from disk);
        old logs without prompt events replay unchanged
      - [x] agent notes injection: `prompts.rs` unit tests (folder
        case-insensitive; **only `AGENT.md` + `SKILL.md` injected** —
        scratch files like TODO.md stay out; per-file/section caps).
        Spec narrowed 2026-10-04 from "load every text file" to the
        two-file whitelist (the folder doubles as scratch workspace).
- [x] **Phase 6 — TUI (ratatui + crossterm)**: COMPLETE (details above)
- [~] **Phase 7 — container + polish**: code COMPLETE; live compaction+resume
      verified; only podman build/run left (sandbox-blocked)
      - [x] `Containerfile` (Fedora 44 + rust/cargo; build release binary in
        image; git identity; `ENTRYPOINT ["harness"]`), `build.sh`, `run.sh`
        (modes: TUI / `once` / `resume`; mounts + explicit state/ws envs +
        HARNESS_TLS_VERIFY=0; seeds config.toml on first run; --add-host for
        the LAN backend)
      - [x] BUG FIX found in review: run.sh TUI mode passed no args ->
        bare `harness` (headless ready) instead of the TUI; now passes `tui`
      - [x] `harness resume "prompt"` headless path (latest session,
        summary-aware replay, one turn) — makes create→compact→resume
        reachable in the binary (Python reference had no headless resume)
      - [x] `harness once "prompt"` alias (plan §5-Q4 CLI contract; "once"
        is stripped, not part of the prompt)
      - [x] run.sh `resume` mode added in the follow-up review (was
        advertised in the README but missing — `./run.sh resume "prompt"
        project` -> `harness resume "prompt"`)
      - [x] `README.md` adapted to the Rust implementation
      - [x] e2e create → resume verified live (one session across separate
        process invocations; files created in each resumed turn)
      - [x] verify live create → COMPACT → resume (qwen_3_8_27B,
        synthetic budget in a throwaway config): compaction fires —
        `summary` events + kept-tail `dup` events land in the right order,
        resume replays the summary, and the over-small-budget abort path
        ("context exceeded" / "compaction made no progress") fires as
        designed. Note: the local estimate is chars/4 of memory contents —
        it deliberately does NOT include the system prompt + tool schemas
        (matches the Python reference), so real-world budgets must stay
        well above the base prompt + schemas (~1.5k tokens) for a session
        to survive compaction.
      - [ ] verify `podman build` + `podman run` — blocked in THIS sandbox
        (rootless, /etc/subuid and /etc/subgid empty -> cannot unpack the
        Fedora base image). On your podman host: `./build.sh` then
        `./run.sh once "create a.txt containing: hi" /tmp/proj`.
- [x] **Phase 8 — backend model management: load / unload / status**
- [x] **CR_01/3 retest fix — Shift+Enter was dead code** (2026-10-05,
      "shift test" session): the `SHIFT+Enter` branch existed and was
      correct, but NO terminal ever reports Shift+Enter distinctly in
      legacy encoding (both send bare `\r`), so the branch was unreachable.
      Fix: enable the kitty keyboard protocol ("disambiguate keys",
      `CSI > 1 u`, `KBD_ENABLE`) at TUI start and reset it with `CSI < u`
      (`KBD_RESET`) on EVERY exit path (normal quit + panic hook), reset
      before leaving the alt screen per the kitty docs. Terminals without
      support ignore the sequence — legacy behavior unchanged there.
      Bonus: this also makes Shift+↑/↓ scrolling actually carry the SHIFT
      modifier (function keys are shift-blind in legacy encoding too).
      Verified live in a pty (30×120 winsize — pty has none by default,
      ratatui renders nothing at 0×0): buffer `ab` + `\x1b[13;2u` (kitty
      Shift+Enter) + `cd` → input buffer exactly `ab\ncd`, rendered with
      the `·` newline marker; `KBD_ENABLE` emitted at start, `KBD_RESET`
      exactly once before `?1049l`. 77 tests green, 0 warnings, 0 unsafe.
      NOTE for future pty probes: this container has `HARNESS_STATE=/state`
      + `HARNESS_WORKSPACE=/workspace` EXPORTED (the real, live session
      state) — always override BOTH in the command line when testing.
      (Two junk sessions 20261005-0125/-0130 in /state/sessions from one
      un-overridden probe were removed.)
- [x] **CR_01/3 SECOND fix — the first fix was wrong** (2026-10-05, user
      retest: "shift+enter still treated like enter"): the previous fix
      pushed ONLY kitty flag 1 (disambiguate, `\x1b[>1u`). The kitty spec
      EXEMPTS Enter from disambiguate — it still sends a bare `\r` even
      with flag 1 on (so the shell can run `reset` after a crash), so
      Shift+Enter was STILL byte-identical to Enter. Fix: push flags
      1+4+8 (`kbd_flags()`, `\x1b[>13u`) — flag 8 (report ALL keys as
      escape codes) is what overrides the Enter exemption, so Shift+Enter
      = `CSI 13;2u` (Enter+SHIFT), plain Enter = `CSI 13u`. Flag 4
      (alternate keys) makes shifted letters decode to the real char
      (crossterm consumes the alt field + strips SHIFT). Flag 2 is
      deliberately OFF (it would make key autorepeats non-Press, breaking
      hold-to-repeat). Used crossterm's `PushKeyboardEnhancementFlags` /
      `PopKeyboardEnhancementFlags` (`CSI <1u`) instead of raw bytes, on
      every exit path (normal + panic). Deliberately NOT implemented the
      kitty "detect-before-enable" handshake: it needs a competing stdin
      reader that races crossterm's event reader (eats first keystrokes);
      unconditional enable is safe (non-kitty terminals ignore the
      sequence). Also added: **Ctrl-J universal newline** (works in EVERY
      terminal — the guaranteed fallback when kitty isn't honored, e.g.
      tmux passthrough off / xterm / stock VTE); approval modal y/a/n
      match made case-insensitive (kitty flag 4 sends `Y` not `y`);
      `/help` + hint row advertise "shift+enter / ctrl-j". Verified in a
      30×120 pty: `CSI >13u` pushed, kitty Shift+Enter ×2 + ab/cd →
      input `·ab·cd`, plain Enter submits `ab\ncd` (real newline in the
      JSONL user msg), kitty `CSI 65u`→`A` + `CSI 106;5u` (Ctrl+J)→`\n` +
      `z` → `A·z` (no stray J), legacy `\x0a` Ctrl+J also works, non-ASCII
      `héllo` preserved, clean /quit → exactly 1 push + 1 pop +
      ?1000l + ?1049l. **COMPLETE** after one more edge found on final
      audit: Caps Lock. The spec sends the shifted alternate key only for
      the SHIFT MODIFIER — Caps Lock is a lock STATE, so `a`+Caps arrived
      as `CSI 97;65u` = Char('a')+CAPS_LOCK and would have inserted
      lowercase. Fixed in pure+tested `input_char(c, modifiers, state)`
      (3 new unit tests: plain/kitty/non-ascii, caps-lock, modifier
      non-interference). Re-verified in pty: kitty `CSI 97;65u` → `A`,
      `CSI 98u` → `b`, legacy `\x0a` newline, Shift+Enter, kitty
      `CSI 99;5u` Ctrl-C stop-turn all OK. 80 tests green (68 lib),
      0 warnings, 0 unsafe.
- [x] **Review pass (plan-alignment)**:
      - [x] plan §5-Q4 subcommands (`harness tui` / `harness once`) honored
      - [x] plan §8 "TUI gets its own subcommand" — done; bare `harness`
        stays the headless ready message
      - [x] plan §4.2 / replay / session schema — verified unchanged
      - [x] FIXED flaky test: `loop_::tests` tmp_session/tmp_cfg dirs used
        pid+nanos and collided under parallel test threads (TOCTOU in
        Session::create -> prompt events leaked between tests). Now a
        process-wide atomic counter names each test's ws + session dir.
- [x] **Reasoning block echoed back to the server** (2026-10-06): the
      Python reference used `reasoning_content` for display only and never
      sent it back; this refactor makes the Rust harness echo the model's
      thinking block in the assistant message so llama.cpp replays it into
      the model's own thinking channel (llama.cpp `common/chat.cpp`
      from_oaicompat parses `reasoning_content` on assistant messages;
      `tools/server/server-task.cpp` renders it back as a thinking block).
      Changes: `loop_.rs::stream` accumulates `SseEvent::Reasoning` into a
      `StreamResult{content, reasoning, tool_calls, usage}`; both
      assistant-message paths (final + tool-calling round) build the wire
      message via `assistant_msg(content, reasoning, tool_calls)` —
      `reasoning_content` present only when non-empty; `Session`
      `record_assistant` + replay `event_to_message` now persist/restore
      `reasoning_content` (resume keeps thinking); `compaction.rs
      record_kept_tail` carries it through the `dup` tail (resume after a
      mid-turn compaction keeps the thinking); `estimate_prompt_tokens`
      counts reasoning chars. Tests: `reasoning_is_echoed_to_memory_jsonl_
      and_replay` (memory + JSONL + resume replay + captured next request
      all carry the block). 81 tests green (69 lib), 0 warnings, 0 unsafe.
- [x] **Four user-reported TUI bugs fixed** (2026-10-07):
      - [x] Shift+Enter treated as Enter → kitty protocol is now DETECTED
        via the startup handshake (`CSI ?1u` + `CSI ?15n`, 100 ms window,
        `kitty_probe` in tui.rs, runs BEFORE the crossterm EventStream and
        before seeding so nothing else races for the reply); full flags
        13 (`CSI >13u`) pushed only when the terminal answers, `CSI <1u`
        popped on every exit path as before. Pure+tested
        `parse_kitty_probe_response` / `find_subseq` (+ empty/short
        haystack guard).
      - [x] Git "filesystem boundary" (GIT_DISCOVERY_ACROSS_FILESYSTEM)
        warning spilling over the UI → new `src/logmsg.rs` sink: all
        runtime eprintln/println (config, seed, gitstore, session, loop)
        go through it; headless prints normally, TUI renders them as `⚠`
        lines in the conversation (`UiEvent::Warn` + warn mpsc channel).
        Also reworked `gitstore::commit_all`: `ensure_repo` no longer
        commits (fixed a double-commit in the self-heal path where a clean
        tree after re-init made the normal flow return None).
      - [x] No visual spacing between message/tool/thinking sections →
        `Section` enum + `App::gap_if` blank-line separators between
        DIFFERENT adjacent sections; tool call + result stay one block;
        stream continuations never gap.
      - [x] Scroll-up jumped to top of log → `user_scroll` is now truly
        "distance from the bottom": pure+tested `scroll_offset(user_scroll,
        total, visible)` + `conv_visible_height()` (terminal − 1+1+3 pinned
        rows); `scroll_up` clamps at the real max.
      - [x] E2E `agent/kitty_e2e.py {kitty,legacy}` (pty + minimal CUP
        screen model, temp state/ws, binary arg): kitty PASSes (reply →
        push, `shift+enter` hint, Shift+Enter + Ctrl-J insert newlines
        without submitting, clean /quit); legacy PASSes (no push,
        `ctrl-j` hint, clean quit; its typed-byte assertion is a NOTE-skip
        in THIS sandbox — the pty drops early bytes, confirmed with a
        plain `echo` probe).
      - [x] 90 tests green (78 lib), 0 warnings, 0 unsafe, release build
        clean. Note: rustfmt is not installed in this sandbox.
- [x] **TUI input box: newlines now render as real rows** (2026-10-07,
      user report "after ctrl+j it still looks like 1 line in my input
      box"): the input was rendered on ONE visible row with `\n` shown as
      a `·` marker. Now `render_input_lines` (pure, unit-tested) returns
      one `Line` per hard buffer line: the box grows with newlines,
      capped at `MAX_INPUT_LINES` (5) with a vertical window scrolled to
      keep the caret's row visible; prompt marker `> ` on the first
      visible row, 2-space indent on continuations; per-row horizontal
      clamp unchanged (caret row's window slides to the caret, `…`
      ellipses). Layout: input block is now `Length(input_rows + 2)` via
      `input_block_lines()` (shared with `conv_visible_height`, which
      now takes the row count instead of the hardcoded 3, so the
      scroll-up clamp stays exact). `render_input_core` removed; the 3
      single-line tests now call `render_input_lines(...)[0]`, the old
      `·`-marker test replaced, +4 new tests (own-row rendering, caret on
      row 2, 8-line window scroll, block-line cap). E2E
      `agent/kitty_e2e.py` updated: reconstructs the multi-line buffer
      from the screen (lowest `> ` row + 2-space-indented continuations,
      hint row terminates) and asserts `ab\ncd\n/quit` as 3 rows —
      kitty PASS, legacy PASS (typed-byte NOTE-skip as before).
      93 tests green (81 lib), 0 warnings, 0 unsafe, release build clean.
- [x] **Refactor: use the backend's STREAMED token counters instead of
      internal counting** (2026-10-09, user report "prompt/completion
      tokens are counted internally, but v1/chat/completions actually
      streams and updates those counts"):
      - [x] REVIEW: the user was right. Two layers, both now backend-driven:
        (a) the OpenAI `usage` object was ALREADY consumed from the
            terminal stream chunk (`stream_options.include_usage`) — the
            per-request final counts were real all along; (b) the llama.cpp
            server ALSO streams LIVE per-chunk counters — a `timings` object
            on every SSE chunk (`predicted_n` = completion so far,
            `prompt_n`+`cache_n` = prompt size; prompt grows live during
            prefill) — but ONLY when the request sets
            `timings_per_token: true` + `return_progress: true` (exactly the
            fields the production payload in
            `reference/llama_cpp_completion_with_thinking_example` sends,
            and verified in the llama.cpp fork source,
            `tools/server/server-task.cpp`). The harness never sent them, so
            the loop emitted `usage` only after each stream finished and
            text/reasoning UI events carried an internal chars/4 estimate.
      - [x] `client.rs::payload`: stream requests now send
          `timings_per_token: true` + `return_progress: true` (standard
          OpenAI backends ignore the extra fields).
      - [x] `client.rs`: new `SseEvent::Usage(Usage)` — the parser emits a
          live usage event from ANY chunk carrying a top-level `usage` or
          `timings` object (new `Usage::from_wire` / `Usage::from_timings`);
          last write wins, so the terminal `Done.usage` is the best
          per-request count (final usage chunk, else last live timings).
      - [x] `loop_.rs::stream(&self, prompt_tokens, completion_tokens)`:
          turn-level counters passed in; live `SseEvent::Usage` updates
          fold into them (prompt = max, completion = pre-request base +
          per-request running max) and emit `usage` UI events MID-STREAM
          (deduped on change so chatty per-token timings don't spam the UI
          channel). Internal `completion_chars / 4` estimate REMOVED from
          text/reasoning events. `run_turn` reconciles to the authoritative
          per-request totals after the stream (idempotent for full streams;
          matters when a stream ends without a terminal usage chunk);
          `estimate_prompt_tokens` survives ONLY as the no-reporting
          fallback + the local budget gate.
      - [x] `tui.rs`: handler logic already matched (prompt = max,
          completion = set) — comments updated; status bar now shows the
          backend's live counters mid-request, not per-request snapshots.
      - [x] Tests: `sse_live_usage_from_timings` (per-chunk timings → live
          events, last one in Done, EOF-without-[DONE] framing),
          `sse_parse_contract` extended (final usage chunk also surfaces
          live), payload test asserts the new stream-only fields,
          `live_usage_events_feed_turn_counters_without_double_counting`
          (live event precedes text; turn totals 100/5 not 1+5; request-end
          reconcile dedupes).
      - [x] VERIFIED LIVE against llm.example.lan (gemma-4-31B): raw curl
          with the harness payload shows per-chunk `timings` streaming
          (prefill prompt_n 0→21, then predicted_n per token); headless
          `harness "Reply with exactly: OK"` (throwaway state/ws) ends with
          real counts `prompt_tokens=1138 completion_tokens=27` in the
          turn_end event.
      - [x] Also fixed a pre-existing FLAKE found while verifying: the two
          `logmsg` tests race on the process-global SINK — now guarded by a
          test-local `SINK_LOCK` mutex (same pattern as `tests/seed.rs`).
      - [x] 95 tests green (83 lib + 5 backend + 5 compaction + 2 seed),
          0 warnings, 0 unsafe, release build clean; 6 consecutive green
          runs after the flake fix.

## 2026-10-09 budget gate now driven by the live server count (user follow-up)

User asked to confirm the summary THRESHOLD + TRIGGER also use the
stream-provided counts. Finding: the gate already consumed the live count
but as `max(live, last, estimate(whole_memory))` — the chars/4 estimate
could still force compaction before the live count crossed the threshold;
and the threshold was computed two subtly different ways (milli-int math
in `check_budget` vs f64 floor in `ensure_budget`). Fixed:

- [x] `config.rs`: `ContextConfig::threshold_tokens()` =
      floor(max_tokens * summarize_threshold), clamped >= 1 — single source
      of truth used by BOTH the turn gate and the `ensure_budget` loop.
      Unit test incl. the clamp.
- [x] `loop_.rs`: new fields `requested_mem_len` (memory len at last
      request — what the server's live count actually covers) +
      `live_usage` (last request reported a server-side prompt count).
      `check_budget` gate is now: live base (`max(in-turn, last turn)`)
      + chars/4 estimate of ONLY the unsent tail (this round's
      assistant+tool group, or the new user message). The whole-memory
      estimate is the gate only when there is no live data (fresh session
      / backend without usage).
- [x] `ensure_budget`'s inner loop keeps the estimate — deliberately: it
      re-checks CANDIDATE memories that were never sent, so no live count
      exists (comment added).
- [x] Tests: new `budget_gate_prefers_live_count_over_estimate` (live 1k
      + tail under threshold while whole-memory estimate is OVER it → no
      summarizer call; old gate fired). `tests/compaction.rs`
      `ChainClient` now reports a realistic growing prompt count (its
      chars/4 of what it was sent) instead of a constant `prompt_tokens: 1`
      — with the old estimate-driven gate the fake never mattered.
- [x] 102 tests green (90 lib + 5 backend + 5 compaction + 2 seed),
      0 warnings, 0 unsafe, release build clean.

## Notes
- Zero `unsafe` in src/. thiserror for BackendError. No .expect() on I/O in
  agent core. Tests: env var setting is unsafe (Rust 2024) — guard with the
  `env_lock` mutex pattern from `tests/seed.rs`.
- tokio multithreaded; async reqwest SSE; CancellationToken for cancel;
  approval = injectable callback (oneshot if async input needed).
- Backend default for this env: https://llm.example.lan/v1 (HARNESS_TLS_VERIFY=0).
- JSONL session schema unchanged (must resume Python-written logs).
- `$HARNESS_STATE/config.toml` must NEVER be touched (prior agent incident).
- Checkpoint state 2026-07-13: 45 tests green, 0 warnings, 0 unsafe.
- Checkpoint state 2026-10-03 (Phase 4 done): 61 tests green (49 lib incl.
  compaction/loop unit tests, 7 backend, 2 seed, 5 compaction integration),
  0 warnings, 0 unsafe. Cancel is a watch-bool (Python Event parity),
  NOT a CancellationToken. `ensure_budget`/`compact_once` take a
  `FindCut` strategy + `Option<&C: Fn() -> bool>` cancel (pass
  `Some(&|| false)` when unused). Smoke-run model note: gemma-4-31B at
  llm.example.lan is slow; use max_tokens ~4000 for smoke runs.

## CR_01: COMPLETE (implemented + tested this session)

**Priority bug ("stream interrupted: error decoding response body"): VERIFIED
already fixed in tree.** `.read_timeout(600)` IDLE deadline (resets per chunk,
never kills a live stream) + `.connect_timeout(10)`, no total deadline
(`client.rs`), plus stall diagnostics in `loop_.rs::stream` (1-min silence
heartbeat naming `READ_TIMEOUT`) and rich error context (model/round/
memory len/prompt_tokens/turn age) written to stderr + session log.

**All 10 CR_01 items done:**
1. **TUI pinned header** — `App.header` (session · model · tools) rendered in
   its own layout row, never scrolls with the doc.
2. **Better scrolling** — PageUp/Down, Shift+↑/↓, AND mouse wheel
   (`EnableMouseCapture` + `Event::Mouse` ScrollUp/ScrollDown); hint row shows
   the keys.
3. **Input box** — multi-line (`insert_input`/`backspace`/`delete`/`move`
   with a char-index cursor), Shift+Enter inserts a newline, long input is
   clamped to the width with a sliding window + `…` ellipses (tail shown when
   the caret is near the end), and a **blinking block cursor** marks the box
   as selected. `render_input_core` is pure + unit-tested (7 cases).
4. **Working label** — `set_working` now reports `thinking` (reasoning) /
   `answering` (text) / `tool: <name>` (tool call) / `waiting for model` /
   `compacting`.
5. **Cumulative, live prompt tokens** — new `Usage{prompt,completion}` emit
   fires after every streaming request (backend usage or the estimate); the
   TUI takes `max(prompt)` across requests and accumulates completion, so the
   status bar updates mid-turn, not just at turn end.
6. **Ctrl-C = stop turn only, never quit** — Ctrl-C idle now prints a hint
   (`/quit` to exit); `q` no longer quits; modals can't quit. `/quit` is the
   only exit.
7. **edit_file spill** — on "not found" / "not unique" the `new_string` is
   saved to `.harness/<base>.edit-<n>` (bumps on repeats) and the error names
   the path; tool description updated so the model knows to recover it with
   `read_file`. Tested.
8. **read_file vision** — image files (png/jpg/gif/webp/bmp/tiff, magic-byte
   verified) return `ToolResult.images` = a base64 data URI; the loop builds
   a parts-array tool message (`text` + `image_url`) on the wire while the
   JSONL audit log keeps the text only (images not persisted — a resume
   re-reads the file). `estimate_prompt_tokens` now counts only `text` parts
   (the base64 blob must not inflate the budget). `record_assistant` now takes
   `&Value` content (still `json!(str)` for text). All tested incl. roundtrip
   decode + misnamed-image fallback + memory-vs-audit shape.
9. **Pretty tool results (display only)** — `pretty_result` pretty-prints a
   JSON object/array in the TUI; the model + JSONL keep the raw text (results
   are already plain text for accuracy). Tested.
10. **Rename harness -> little_helper** — the user-facing **binary** is now
    `little_helper` (`[[bin]] name`), and Containerfile/build.sh/run.sh/
    README/`main.rs` usage strings + the auto-commit prefix all use it. The
    library crate stays `harness` (internal; referenced as `harness::`) and the
    `HARNESS_STATE`/`HARNESS_WORKSPACE`/`HARNESS_TLS_VERIFY` env contract is
    unchanged (hard rule + cross-language parity).

Checkpoints after CR_01: **77 tests green (65 lib unit tests + 5 backend +
2 seed + 5 compaction integration), 0 warnings, 0 unsafe.**
TUI re-verified in a pty (renders new 4-row layout, clean quit).

## CR_01 (agent/CR_01.md) — reviewed 2026-10-05, work PAUSED for stream bug

Reviewed all items; implementation plan (on hold until the stream fix lands):
1. TUI: lock the session/model/tools header (own row, never scrolls with doc)
2. TUI: better old-chat scrolling (PageUp/Down exists; add wheel/more)
3. TUI input box: wrap/clamp long input; Shift+Enter inserts newline
   (retest 2026-10-05: the handler existed but was dead — legacy terminals
   can't distinguish Shift+Enter from Enter; FIXED by enabling the kitty
   keyboard protocol `CSI > 1 u` at TUI start + reset on every exit path;
   see "CR_01/3 retest fix" above); visible cursor hint
4. TUI: working label shows what the model is doing (thinking /
   writing tool call `name`)
5. TUI: prompt tokens cumulative + update during the turn (live usage)
6. TUI: Ctrl-C = stop the turn only (never quits the app)
7. edit_file: on failure, spill new_string to a temp file and tell the
   model (reuse), update tool description
8. read_file: vision (llama.cpp server accepts image_url: base64 data
   URI / file path) — needs wire-format change (content parts)
9. (nice) pretty-print tool results for display
10. (low) rename harness -> little_helper

### PRIORITY BUG: "stream interrupted: error decoding response body"
Root cause + analysis logged 2026-10-05:
- reqwest client used `.timeout(300)` = TOTAL deadline on the whole
  response (headers + full body). Any stream that takes >300s (long
  generation with a slow model, or >5min reasoning pause with no bytes)
  gets killed mid-body; hyper then surfaces the cancelled body read as
  "error decoding response body". Server side sees h2 RST_STREAM =
  "Connection handling cancelled".
- Evidence: /state/sessions/20261004-1520-coder.jsonl has 4 such errors,
  mostly at ~30-40k-token contexts on qwen_3_8_27B (slow model); one at
  ~3k tokens (server stalled >5min with no bytes); one
  "peer closed without complete body" = server hung up.
- FIX: client timeout policy -> `.connect_timeout(10)` +
  `.read_timeout(600)` (IDLE deadline: resets on every received chunk,
  never kills an actively streaming response). No total deadline.
- ALSO: log backend errors with context (session, model, context size,
  elapsed) to stderr + session log so future stalls are diagnosable.
- [x] implement + test — DONE (verified in tree this session; see
      "CR_01: COMPLETE" above)
- [x] then resume CR_01 items 1-10 above — DONE (all 10 implemented + tested)
- [x] **TUI UX pass — user request visible, real turn counter, clearer
      styling** (2026-10-09, user report: "the TUI does not display the
      user request", "it always shows turn 1", "hard to separate which
      is the agent message"):
      - [x] USER REQUEST IS NOW DISPLAYED: `start_turn` pushes the
            submitted prompt into the conversation — `❯ ` marker, BOLD
            GREEN on the first line, plain green with 2-space indent on
            continuation lines (pure + tested `user_request_lines`).
            New `Section::User` so the separators keep it apart from the
            turn header and the answer. Display only — the loop still
            records the user msg to the JSONL.
      - [x] TURN COUNTER WAS STUCK AT 1: the App keeps its OWN Session
            snapshot (opened once at startup) while the loop's private
            Session is the one that records `turn_end` events — the app
            copy never advanced. `start_turn` is now async and reads
            `self.loop_.lock().await.session.turn_number() + 1` (the same
            expression the loop uses when it records the turn), so the
            header shows the real next turn number, also correct after
            /new and on resumed sessions.
      - [x] STYLING — sections now read apart:
            - agent text: FIRST line of each text block gets a `▸ ` block
              marker (append_text detects block starts vs continuation
              lines via `last_line_is_stream` — a continuation is only
              when the last doc line is still the SAME stream's line; a
              reasoning line, discrete event, or section gap closes the
              block). Marker is per-block, not per-paragraph.
            - tool RESULT BODIES are now DIM (DarkGray; the ✓/✗ mark
              keeps green/red) — the agent's default-color text is now
              the only full-brightness unmarked content.
            - turn header is BOLD (dark gray) so "─ turn N ─" stands out
              above the user line.
            Resulting grammar: `─ turn N ─` (bold gray) / `❯ request`
            (bold green, `  ` continuations) / `▸ answer` (default) /
            dim-italic thinking / cyan `⚙ tool` + dim result / dim
            notes — each separated by the existing blank-line gaps.
      - [x] Tests: 3 existing exact-doc assertions updated for the `▸ `
            marker; +4 new (`agent_text_marker_starts_each_block_only`,
            `agent_text_continues_across_deltas_without_remarking`,
            `user_request_lines_mark_and_indent`,
            `turn_header_counts_from_loop_session_and_shows_request` —
            the last uses `blocking_lock` + recorded turn_end events + a
            throwaway current-thread runtime; the spawned turn fails
            fast against the unreachable default backend).
      - [x] 99 tests green (87 lib + 5 backend + 5 compaction + 2 seed),
            0 warnings, 0 unsafe in src/. E2E `agent/kitty_e2e.py
            {kitty,legacy}` target/debug: both PASS (legacy typed-byte
            NOTE-skip as before).
- [x] **TUI input box: long lines soft-wrap instead of clamping**
      (2026-10-09, user report "if a line is too long before newline, the
      newline isn't visible in the textbox at all — made a lot of typos
      because I can't see what's in there"):
      - [x] Root cause: each hard buffer line rendered exactly ONE visible
            row; anything wider than the box was hidden behind the old
            clamp-to-width + `…` sliding window. After a Shift+Enter at
            the end of an over-long line, the window snapped back to
            column 0, so the line's end (and the newline) were invisible —
            and typos anywhere outside the `avail`-wide caret window were
            too.
      - [x] Fix: `render_input_lines` now SOFT-WRAPS each hard line into
            `avail`-char visible rows (prompt `> ` / indent `  ` take the
            other 2 columns), so the full line — and every newline — is
            always visible. `input_block_lines(input, width)` now counts
            the wrapped rows (`ceil(len/avail)` per hard line) so the
            layout + `conv_visible_height`/scroll-up math agree; the box
            still caps at `MAX_INPUT_LINES` (5) and the vertical window
            scrolls to the caret's VISIBLE row (wrap rows included).
            Caret at the exact end of a wrapped line parks on that line's
            last row past its end (naive `div_ceil` would spill into the
            next line's row).
      - [x] Tests: 2 old clamp/ellipsis tests replaced
            (`render_input_long_line_wraps_and_stays_fully_visible` —
            joined row bodies == the visible tail, no `…`, rows fit the
            box; `render_input_caret_midline_lands_on_wrapped_row` —
            caret on the right wrap row, 4 spans with blink), 1 new
            (`render_input_newline_after_long_line_is_visible` — 40-char
            line + `\n` + `y` renders as 18/18/4/x-y rows, the newline is
            its own visible row), `input_block_lines` test updated for the
            width arg + wrap counts.
      - [x] 100 tests green (88 lib + 5 backend + 5 compaction + 2 seed),
            0 warnings, 0 unsafe in src/, release build clean.
- [x] **TUI: black screen at boot until the first click** (2026-10-09,
      user report "when the tool is first boot the screen is black, once
      you click any button the real ui shows up"):
      - [x] Investigation: pty byte-capture proved the first frame is
            painted ~100 ms AFTER the first key event and NEVER before.
            Milestone logging + /proc thread/syscall inspection showed
            startup stuck between the last two log lines — exactly where
            `prepare_terminal`'s throwaway `current_thread` tokio runtime
            (built for the kitty probe) is DROPPED at function return.
            Minimal repro (standalone binary in a pty): build a
            current_thread (or multi_thread) runtime, `block_on` a
            `tokio::io::stdin()` read with a 100 ms timeout, then
            `drop(rt)` → the drop BLOCKS INDEFINITELY (main thread
            parked in a futex, IO-driver thread idle in epoll; a
            sleep-only runtime drops fine, so the tty registration is
            the trigger). In the real TUI the first keystroke completes
            the IO driver's shutdown and startup continues — hence "the
            UI appears when I press a button". (Earlier confusion: the
            probe's 100 ms tokio timeout DOES fire; the hang is in the
            DROP, not the probe.)
      - [x] Fix: `kitty_probe` is now SYNCHRONOUS — nix `poll` + `read`
            on fd 0 against the same 100 ms deadline (nix features
            `poll`+fs added to the existing dep; zero unsafe). No
            runtime to drop in `prepare_terminal` at all; the flag push
            is a plain sync `execute`. Probe ordering is unchanged
            (before seeding, before the crossterm EventStream reads
            stdin, so nothing races for the reply). The UI's
            multi_thread runtime never registers the tty (crossterm
            EventStream has its own mio reader; the only tokio-io use
            left is shell-process pipes), so its drop at /quit stays
            clean.
      - [x] New regression E2E `agent/first_paint_e2e.py <bindir>`: pty,
            NO input for 3 s, first frame must appear (now at ~0.23 s:
            100 ms probe + first 100 ms tick), then clean /quit exit.
            (Run e2e scripts with the BINARY'S dir, e.g.
            `target/release` — a stale/wrong path fails exec in the pty
            child and every assertion "fails" for no reason.)
      - [x] 100 tests green (88 lib + 5 backend + 5 compaction + 2 seed),
            0 warnings, 0 unsafe in src/, release build clean. E2E:
            first_paint PASS (frame at 0.23 s, clean quit), kitty PASS
            (reply detected → `CSI >13u` push, shift+enter hint,
            newlines, clean /quit), legacy PASS (no push, ctrl-j hint).
- [ ] **PARKED (2026-10-15, user request — not started): sensitive-info
      safeguards on the auto-commit/push path.** Context: repo is now
      public on GitHub (history squashed via orphan branch; old branch
      `master` kept local-only as backup). Hard rule 7 in AGENT.md covers
      the "please" level; user wants a mechanical block. Brainstorm
      (no code written):
      - KEY SEAM: `gitstore::commit_all` shells out to the `git` CLI
            (no `--no-verify`) → plain git hooks ALREADY fire on every
            agent auto-commit, zero Rust changes.
      - L1: `.git/hooks/pre-commit` scanner over the staged diff, shared
            committed denylist file (single source of truth). Patterns
            must be allowlist-aware (placeholder `llm.example.lan` is in
            the repo — a blanket "any .lan" rule self-trips). On hit:
            commit fails → harness self-heal path logs a `⚠` in the TUI,
            file never enters history, agent continues (graceful).
      - L2: `.git/hooks/pre-push` scanning the full `origin/...HEAD`
            diff — the actual egress block; catches cp'd state files and
            rewritten history.
      - L3 (net, optional): GitHub secret-scanning push protection +
            CI gitleaks on push.
      - L4 (structural): secrets mounted read-only OUTSIDE /workspace +
            /state; assert in run.sh that the state repo NEVER has a
            remote.
      - L5 (runbook): one-command public-history reset script (orphan →
            commit → force-push, the move done 2026-10-15) + on-demand
            full-history `gitleaks` sweep.
      - Open design question: on L1 hit — hard-refuse (recommended,
            graceful via warn) vs quarantine (move file to `.quarantine/`
            + notify model; friendlier to a retrying model).
      - User: park it; pick it up when asked.
