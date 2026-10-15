//! Binary entry point: seed state, load config, print a summary.
//!
//! Subcommands (growing across phases):
//!   little_helper "prompt"    run one headless turn (phase 4)
//!   little_helper tui         TUI (phase 6)

use std::path::Path;

use anyhow::Result;
use serde_json::Value;

fn main() -> Result<()> {
    // Development-safety guard: refuse to run against the container's real
    // global state/workspace. The production run.sh (and every CI/test)
    // always sets BOTH HARNESS_STATE and HARNESS_WORKSPACE to explicit
    // paths; a bare `cargo run` must never seed /state or git-commit the
    // real repo. This is deliberate — there is no "safe default" that is
    // also a real mount point, so we opt in rather than guess.
    let (state, ws) = (
        harness::config::state_dir(),
        harness::config::default_workspace_path(),
    );
    match (
        std::env::var_os("HARNESS_STATE"),
        std::env::var_os("HARNESS_WORKSPACE"),
    ) {
        (Some(_), Some(_)) => {
            // explicit: trust the caller (container / CI / dev-with-tempdirs)
        }
        _ => {
            let (s_bad, w_bad) = (
                state == std::path::Path::new("/state") || state.exists(),
                ws == "/workspace" || Path::new(&ws).exists(),
            );
            if s_bad || w_bad {
                anyhow::bail!(
                    "refusing to run without explicit HARNESS_STATE + HARNESS_WORKSPACE \
                     (would touch global state={state:?} / workspace={ws:?}). \
                     Point both at throwaway dirs, e.g. \
                     HARNESS_STATE=/tmp/harness-state HARNESS_WORKSPACE=/tmp/ws cargo run"
                );
            }
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("tui") {
        let cfg = prepare();
        harness::tui::run(cfg)?;
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("resume") {
        // `harness resume "prompt"` — reopen the most recent session, replay
        // it (summary-aware), and run one more turn in it. This is the
        // headless resume path (plan §7 end-to-end check: create -> compact
        // -> resume). Args after "resume" are joined as the prompt.
        let rest = args.iter().skip(1).collect::<Vec<_>>();
        if rest.is_empty() {
            eprintln!("usage: little_helper resume \"prompt\"");
            std::process::exit(2);
        }
        let cfg = prepare();
        let prompt = rest
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let Some(path) = latest_session(&cfg) else {
            eprintln!("no session to resume in {:?}", harness::config::state_dir());
            std::process::exit(2);
        };
        let code = run_headless_resumed(&cfg, &path, &prompt);
        std::process::exit(code);
    }
    if args.is_empty() {
        // No prompt: seed + config summary + ready message (Python parity).
        let cfg = prepare();
        print_config(&cfg);
        println!("Headless agent loop ready — pass a prompt to run a turn: little_helper \"do something\"");
        return Ok(());
    }
    // One-shot turn: `harness "prompt words"` (args joined, like Python's
    // `" ".join(sys.argv[1:])`). `harness once "prompt"` is an alias — the
    // "once" is stripped so it is not part of the prompt.
    let args: Vec<String> = if args.first().map(String::as_str) == Some("once") {
        args.iter().skip(1).cloned().collect()
    } else {
        args
    };
    if args.is_empty() {
        eprintln!("usage: little_helper [tui | resume \"prompt\" | once \"prompt\" | \"prompt\"]");
        std::process::exit(2);
    }
    let cfg = prepare();
    print_config(&cfg);
    let prompt = args.join(" ");
    let code = run_headless(&cfg, &prompt);
    std::process::exit(code);
}

/// Seed + config load with reload-on-seed (port of `__main__.py::main`).
fn prepare() -> harness::config::Config {
    harness::tools::register_all();
    let cfg = harness::config::load_config();
    if harness::state::seed::seed(&cfg) {
        // config file was just written; the next run picks up its values
        return harness::config::load_config();
    }
    cfg
}

/// One headless turn. Returns the process exit code (1 on !ok).
fn run_headless(cfg: &harness::config::Config, prompt: &str) -> i32 {
    let rt = tokio::runtime::Runtime::new().expect("valid tokio runtime");
    rt.block_on(async {
        use harness::agent::loop_::AgentLoop;
        use harness::agent::session::Session;
        use harness::backend::BackendClient;

        let session = Session::create("coder", &cfg.backend.model, None);
        eprintln!("session {} -> {}", session.id(), session.path.display());
        let client = std::sync::Arc::new(BackendClient::new(cfg));
        // Headless approval: preview + [y]/[n]/[a] on stderr, read one
        // stdin line. EOF denies — unattended runs never hang.
        let ask: std::sync::Arc<dyn Fn(&str, &Value, &str) -> String + Send + Sync> =
            std::sync::Arc::new(|name: &str, _args: &Value, preview: &str| {
                eprintln!("===== {name} — approval requested =====");
                eprintln!("{preview}");
                let mut answer = String::new();
                eprint!("[y]es / [n]o / [a]lways-allow this tool: ");
                use std::io::Write;
                let _ = std::io::stderr().flush();
                let Ok(n) = std::io::stdin().read_line(&mut answer) else {
                    return "n".to_string();
                };
                if n == 0 {
                    return "n".to_string();
                }
                match answer.trim().to_lowercase().as_str() {
                    "y" | "yes" => "y".to_string(),
                    "a" | "always" => "a".to_string(),
                    _ => "n".to_string(),
                }
            });
        let mut loop_ = AgentLoop::new(
            cfg.clone(),
            client,
            session,
            Some(ask),
            None,
        );
        let result = loop_.run_turn(prompt).await;
        let session_id = loop_.session.id();
        if !result.ok {
            eprintln!("turn failed: {}", result.reason);
            return 1;
        }
        let mut bits = vec!["turn ok".to_string(), format!("session={session_id}")];
        if let Some(c) = &result.commit {
            bits.push(format!("workspace@{c}"));
        }
        bits.push(format!("prompt_tokens={}", result.prompt_tokens));
        bits.push(format!("completion_tokens={}", result.completion_tokens));
        println!("{}", bits.join("  "));
        0
    })
}

/// Most recent session file in the state dir (by mtime), if any.
fn latest_session(cfg: &harness::config::Config) -> Option<std::path::PathBuf> {
    let _ = cfg;
    let dir = harness::config::state_dir().join("sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return None;
    };
    let mut best: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(md) = e.metadata() else {
            continue;
        };
        let mt = match md.modified() {
            Ok(mt) => mt,
            Err(_) => continue,
        };
        match &best {
            Some((b, _)) if mt <= *b => {}
            _ => best = Some((mt, p)),
        }
    }
    best.map(|(_, p)| p)
}

/// One headless turn on a resumed session (replay first — the ported
/// replay contract guarantees the loop's memory rebuilds to the recorded
/// state, including any phase-5 `prompt` overrides).
fn run_headless_resumed(cfg: &harness::config::Config, session_path: &std::path::Path, prompt: &str) -> i32 {
    let rt = tokio::runtime::Runtime::new().expect("valid tokio runtime");
    rt.block_on(async {
        use harness::agent::loop_::AgentLoop;
        use harness::agent::session::Session;
        use harness::backend::BackendClient;

        let session = Session::open(session_path);
        eprintln!("resuming session {} -> {}", session.id(), session.path.display());
        let client = std::sync::Arc::new(BackendClient::new(cfg));
        let ask: std::sync::Arc<dyn Fn(&str, &Value, &str) -> String + Send + Sync> =
            std::sync::Arc::new(|name: &str, _args: &Value, preview: &str| {
                eprintln!("===== {name} — approval requested =====");
                eprintln!("{preview}");
                let mut answer = String::new();
                eprint!("[y]es / [n]o / [a]lways-allow this tool: ");
                use std::io::Write;
                let _ = std::io::stderr().flush();
                let Ok(n) = std::io::stdin().read_line(&mut answer) else {
                    return "n".to_string();
                };
                if n == 0 {
                    return "n".to_string();
                }
                match answer.trim().to_lowercase().as_str() {
                    "y" | "yes" => "y".to_string(),
                    "a" | "always" => "a".to_string(),
                    _ => "n".to_string(),
                }
            });
        let mut loop_ = AgentLoop::new(cfg.clone(), client, session, Some(ask), None);
        let result = loop_.run_turn(prompt).await;
        let session_id = loop_.session.id();
        if !result.ok {
            eprintln!("turn failed: {}", result.reason);
            return 1;
        }
        let mut bits = vec!["turn ok (resumed)".to_string(), format!("session={session_id}")];
        if let Some(c) = &result.commit {
            bits.push(format!("workspace@{c}"));
        }
        bits.push(format!("prompt_tokens={}", result.prompt_tokens));
        bits.push(format!("completion_tokens={}", result.completion_tokens));
        println!("{}", bits.join("  "));
        0
    })
}

fn print_config(cfg: &harness::config::Config) {
    let ask: Vec<&String> = cfg
        .tools
        .approval
        .iter()
        .filter(|(_, lvl)| lvl.as_str() == "ask")
        .map(|(n, _)| n)
        .collect();
    println!("little_helper config");
    println!(
        "  backend    {}  model={}",
        cfg.backend.base_url, cfg.backend.model
    );
    println!(
        "  context    max={}  summarize={} (threshold={}, keep={})",
        cfg.context.max_tokens,
        cfg.context.summarize.as_str(),
        cfg.context.summarize_threshold,
        cfg.context.keep_recent_turns
    );
    println!(
        "  workspace  {}  auto_git={}",
        cfg.workspace.path, cfg.workspace.auto_git
    );
    println!(
        "  tools      enabled={}  ask={:?}",
        cfg.tools.enabled, ask
    );
    println!(
        "  search     engine={}  max_results={}",
        cfg.search.engine, cfg.search.max_results
    );
}
