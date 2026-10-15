//! run_command: execute shell commands in the workspace.
//!
//! When `limits.exec_network` is false, commands run inside a fresh network
//! namespace (`unshare -n`). If unshare is unavailable we refuse to run with
//! network instead of silently relaxing the sandbox.
//!
//! Concurrency (plan §2): `tokio::process::Command` with a **new process
//! group** (`.process_group(0)`, a safe `setpgid` — the child's PID becomes
//! its PGID), a `tokio::time::timeout`, and on expiry we kill the **whole
//! group** with the safe `nix::killpg`. Zero `unsafe`. The PID-1 /proc
//! reaper (below) still collects grandchildren that get re-parented to us
//! after the group kill.

use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::sys::wait::{WaitPidFlag, waitpid};
use nix::unistd::Pid;
use serde_json::json;
use tokio::io::AsyncReadExt;

use super::registry::{cap, register, ToolContext, ToolResult};

static REAPER: std::sync::Once = std::sync::Once::new();

/// Start (once, and only as PID 1) a daemon thread that reaps orphans.
///
/// Port of Python's `_ensure_reaper`, same rationale: as container PID 1 we
/// must wait() re-parented orphans or they linger as zombies. Only zombies
/// that are (a) dead children of us and (b) *not* session leaders are
/// touched — the direct run_command child is reaped by tokio itself, and
/// its re-parented grandchildren (which keep a group id != their pid) are
/// the only ones collected here.
fn ensure_reaper() {
    REAPER.call_once(|| {
        if std::process::id() != 1 {
            return; // a real parent reaps our orphans for us
        }
        // Detach the handle: the thread lives to process exit (a Rust thread
        // is a daemon thread by that measure — matching Python's daemon
        // thread; the process never waits on it at exit).
        let _ = std::thread::Builder::new().name("harness-reaper".into()).spawn(|| {
            loop {
                std::thread::sleep(Duration::from_millis(500));
                let Ok(entries) = std::fs::read_dir("/proc") else {
                    continue;
                };
                for entry in entries.flatten() {
                    let file_name = entry.file_name();
                    let Some(pid_str) = file_name.to_str() else {
                        continue;
                    };
                    let Ok(pid) = pid_str.parse::<i32>() else {
                        continue;
                    };
                    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                        continue;
                    };
                    // Fields after the closing ')' of comm (which may
                    // contain spaces): state(0) ppid(1) pgrp(2) ...
                    let Some(rest) = stat.rsplit_once(')') else {
                        continue;
                    };
                    let fields: Vec<&str> = rest.1.split_whitespace().collect();
                    if fields.len() < 3 || fields[0] != "Z" || fields[1] != pid_str {
                        continue; // not a dead child of ours
                    }
                    let Ok(pgrp) = fields[2].parse::<i32>() else {
                        continue;
                    };
                    if pgrp == pid {
                        continue; // session/group leader: tokio reaps it
                    }
                    let _ = waitpid(Some(Pid::from_raw(pid)), Some(WaitPidFlag::WNOHANG));
                }
            }
        });
    });
}

fn shell_argv(command: &str, sandboxed: bool) -> Vec<String> {
    if sandboxed {
        vec!["unshare".into(), "-n".into(), "bash".into(), "-c".into(), command.into()]
    } else {
        vec!["bash".into(), "-c".into(), command.into()]
    }
}

/// Run argv in the workspace: new process group, piped stdout+stderr,
/// `tokio::time::timeout` -> kill the whole group + bounded second wait.
async fn run(argv: &[String], ctx: &ToolContext, command_display: &str) -> (String, bool) {
    let timeout = Duration::from_secs(ctx.limits().exec_timeout_sec);
    ensure_reaper();

    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(&ctx.workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // New process group (safe setpgid): the child's PID becomes its PGID,
        // so `killpg(pid)` reaches its whole subtree and only its subtree.
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (format!("error: cannot start {}: {e}", argv[0]), false),
    };
    let pid = match child.id() {
        Some(id) => id,
        None => return (format!("error: no pid for {}", argv[0]), false),
    };

    // Drain stdout and stderr on their own tasks so a big output can never
    // deadlock us while we time the process (Python merges stderr into the
    // stdout pipe via dup2; we capture the two streams and concatenate —
    // cosmetic ordering difference only, same content).
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out_task = tokio::spawn(async move {
        let mut b = String::new();
        let _ = stdout.read_to_string(&mut b).await;
        b
    });
    let err_task = tokio::spawn(async move {
        let mut b = String::new();
        let _ = stderr.read_to_string(&mut b).await;
        b
    });

    let (code, suffix) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => (status.code().unwrap_or(0), String::new()),
        Ok(Err(e)) => {
            return (format!("error: wait failed: {e}"), false);
        }
        Err(_) => {
            // Timeout: kill the whole process group, then the direct child
            // (belt and suspenders). The group only exists because we made
            // it, so this cannot reach the harness itself.
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
            let _ = child.kill().await;
            // Bounded second wait: a survivor that held a pipe can't hang us
            // here; the reaper collects grandchildren re-parented to us.
            let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
            (
                124,
                format!(" (killed after {}s)", ctx.limits().exec_timeout_sec),
            )
        }
    };

    // Drain the captured output (bounded: readers finish once the pipes close
    // after the group is dead).
    let (out_r, err_r) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(5), out_task),
        tokio::time::timeout(Duration::from_secs(5), err_task),
    );
    let out_s = out_r.ok().and_then(|r| r.ok()).unwrap_or_default();
    let err_s = err_r.ok().and_then(|r| r.ok()).unwrap_or_default();
    let output = format!("{out_s}{err_s}").trim().to_string();
    let body = if output.is_empty() {
        "(no output)".to_string()
    } else {
        cap(&output, ctx.limits().exec_output_chars)
    };
    (
        format!("exit {code}{suffix}\n${command_display}\n{body}"),
        code == 0,
    )
}

async fn _run_command(args: serde_json::Value, ctx: ToolContext) -> ToolResult {
    let Some(command) = args.get("command").and_then(serde_json::Value::as_str) else {
        return ToolResult::err("error: command is required");
    };
    if command.is_empty() {
        return ToolResult::err("error: command is required");
    }
    let command_display = cap(command, 500);
    let limits = ctx.limits();
    if limits.exec_network {
        let (text, ok) = run(&shell_argv(command, false), &ctx, &command_display).await;
        return if ok {
            ToolResult::res(text)
        } else {
            ToolResult::err(text)
        };
    }
    // Sandboxed: probe first, refuse (not relax) if the sandbox is unusable.
    let probe = tokio::process::Command::new("unshare")
        .args(["-n", "true"])
        .output()
        .await;
    let Ok(p) = probe else {
        return ToolResult::err(
            "error: exec_network=false but unshare is unavailable; refusing to run with network",
        );
    };
    if !p.status.success() {
        let detail = String::from_utf8_lossy(&p.stderr).trim().to_string();
        return ToolResult::err(format!(
            "error: network sandbox unavailable ({detail}); refusing to run with network"
        ));
    }
    let (text, ok) = run(&shell_argv(command, true), &ctx, &command_display).await;
    if ok {
        ToolResult::res(text)
    } else {
        ToolResult::err(text)
    }
}

pub fn register_all() {
    register(
        "run_command",
        "Run a shell command in the workspace. Working directory is the workspace root; venv and system tools are on PATH. Network availability follows the exec_network limit. Long output is truncated.",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command line to run"}
            },
            "required": ["command"]
        }),
        |args, ctx| Box::pin(_run_command(args, ctx)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LimitsConfig, ToolsConfig, WorkspaceConfig};
    use serde_json::json;
    use std::path::PathBuf;

    fn setup(name: &str, network: bool, timeout: u64) -> (PathBuf, ToolContext) {
        let ws = std::env::temp_dir().join(format!("harness-shell-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        let mut cfg = Config {
            workspace: WorkspaceConfig {
                path: ws.to_string_lossy().into(),
                auto_git: false,
            },
            tools: ToolsConfig {
                limits: LimitsConfig {
                    exec_timeout_sec: timeout,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.tools.limits.exec_network = network;
        let ctx = ToolContext {
            cfg,
            workspace: ws.canonicalize().unwrap(),
        };
        (ws, ctx)
    }

    #[tokio::test]
    async fn simple_command_captures_output() {
        let (_ws, ctx) = setup("simple", true, 30);
        let r = _run_command(json!({"command": "echo hello; echo err >&2; true"}), ctx).await;
        assert!(r.ok, "{}", r.result);
        assert!(r.result.starts_with("exit 0\n$"), "got: {}", r.result);
        assert!(r.result.contains("hello"), "got: {}", r.result);
        assert!(r.result.contains("err"), "stderr captured: {}", r.result);
    }

    #[tokio::test]
    async fn nonzero_exit_is_error_result() {
        let (_ws, ctx) = setup("exit", true, 30);
        let r = _run_command(json!({"command": "false"}), ctx).await;
        assert!(!r.ok);
        assert!(r.result.starts_with("exit 1\n$"), "got: {}", r.result);
    }

    #[tokio::test]
    async fn timeout_kills_whole_group() {
        let (ws, ctx) = setup("timeout", true, 1);
        // grandchild keeps running; killing only bash would leak it
        let r =
            _run_command(json!({"command": "bash -c 'sleep 30 & exec sleep 30'"}), ctx).await;
        assert!(!r.ok);
        assert!(r.result.contains("killed after 1s"), "got: {}", r.result);
        // give the group kill a moment, then ensure no sleep 30 from us lingers
        tokio::time::sleep(Duration::from_secs(1)).await;
        let ps = tokio::process::Command::new("ps")
            .args(["-eo", "pid,ppid,args"])
            .output()
            .await
            .unwrap();
        let ps = String::from_utf8_lossy(&ps.stdout);
        let lingering: Vec<&str> = ps
            .lines()
            .filter(|l| l.contains("sleep 30"))
            .collect();
        assert!(lingering.is_empty(), "leaked grandchildren: {lingering:?} in:\n{ps}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn output_is_capped() {
        let (_ws, mut ctx) = setup("cap", true, 30);
        ctx.cfg.tools.limits.exec_output_chars = 100;
        let r = _run_command(json!({"command": "yes abc | head -100"}), ctx).await;
        assert!(r.result.contains("[truncated at 100 chars]"), "got: {}", r.result);
    }

    #[tokio::test]
    async fn empty_command_is_error() {
        let (_ws, ctx) = setup("empty", true, 30);
        let r = _run_command(json!({"command": ""}), ctx).await;
        assert!(!r.ok && r.result.contains("command is required"));
    }

    #[tokio::test]
    async fn sandboxed_run_or_refuse() {
        // unshare -n needs privileges that container sandboxes often deny
        // even to root (seccomp). Whatever the environment, the contract is:
        // sandbox works, or we REFUSE — never silently relax to network.
        let probe = tokio::process::Command::new("unshare")
            .args(["-n", "true"])
            .output()
            .await
            .expect("unshare exists");
        let (_ws, ctx) = setup("sandbox", false, 30);
        if probe.status.success() {
            let r = _run_command(
                json!({"command": "python3 -c 'import socket; socket.create_connection((\"8.8.8.8\", 53), 3)' 2>/dev/null && echo NETUP || echo NETDOWN"}),
                ctx,
            )
            .await;
            assert!(r.ok, "sandboxed run should work: {}", r.result);
            assert!(
                r.result.contains("NETDOWN"),
                "network must be unreachable in the sandbox: {}",
                r.result
            );
        } else {
            let r = _run_command(json!({"command": "echo hi"}), ctx).await;
            assert!(
                !r.ok && r.result.contains("refusing to run with network"),
                "must refuse, not relax: {}",
                r.result
            );
        }
    }
}
