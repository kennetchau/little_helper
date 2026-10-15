//! Tool registry: schemas, dispatch, path jail, output caps.
//!
//! Tools return [`ToolResult`] — errors are data for the model, never
//! panics. The dispatch catch is the only last-resort guard in the tools
//! layer (Python's `except Exception` -> error result).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{json, Value};

use crate::config::Config;

/// A tool result: `result` text for the model, `ok` for the harness
/// (e.g. deciding what to commit / display), and optional vision parts
/// (image data URIs the loop attaches to the tool message, item CR_01/8).
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub result: String,
    pub ok: bool,
    pub images: Vec<String>,
}

impl ToolResult {
    pub fn res(text: impl Into<String>) -> Self {
        Self {
            result: text.into(),
            ok: true,
            images: Vec::new(),
        }
    }
    pub fn err(text: impl Into<String>) -> Self {
        Self {
            result: text.into(),
            ok: false,
            images: Vec::new(),
        }
    }
}

/// Boxed tool body (dyn-safe: a trait object cannot directly return a
/// generic `Future`, so we box it). Tools receive an owned context clone so
/// the future is 'static and can hop threads.
type ToolFn =
    Arc<dyn Fn(Value, ToolContext) -> std::pin::Pin<Box<dyn Future<Output = ToolResult> + Send>> + Send + Sync>;

#[derive(Clone)]
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    pub fn_: ToolFn,
}

pub struct RegistryInner {
    /// Insertion (registration) order.
    order: Vec<String>,
    tools: std::collections::HashMap<String, Tool>,
}

pub static REGISTRY: LazyLock<Mutex<RegistryInner>> = LazyLock::new(|| {
    Mutex::new(RegistryInner {
        order: Vec::new(),
        tools: std::collections::HashMap::new(),
    })
});

/// Register a tool. Panics on duplicate names (a bug, not user input).
pub fn register<F, Fut>(name: &'static str, description: &'static str, parameters: Value, f: F)
where
    F: Fn(Value, ToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ToolResult> + Send + 'static,
{
    let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        !reg.tools.contains_key(name),
        "tool {name:?} registered twice"
    );
    reg.tools.insert(
        name.to_string(),
        Tool {
            name,
            description,
            parameters,
            fn_: Arc::new(move |args, ctx| Box::pin(f(args, ctx))),
        },
    );
    reg.order.push(name.to_string());
}

/// Registration-ordered names the model may see: tools on, approval != off.
pub fn enabled_names(cfg: &Config) -> Vec<&'static str> {
    if !cfg.enabled_tools() {
        return Vec::new();
    }
    let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    reg.order
        .iter()
        .map(|s| reg.tools[s].name) // &'static str from the Tool
        .filter(|n| cfg.tools.approval.get(*n).map(String::as_str) != Some("off"))
        .collect()
}

/// OpenAI `tools` array for the backend request.
pub fn schemas(cfg: &Config) -> Vec<Value> {
    enabled_names(cfg)
        .into_iter()
        .map(|n| {
            let t = &REGISTRY.lock().unwrap_or_else(|e| e.into_inner()).tools[n];
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

pub fn approval_for<'a>(name: &str, cfg: &'a Config) -> &'a str {
    cfg.tools
        .approval
        .get(name)
        .map(String::as_str)
        .unwrap_or("auto")
}

/// Run a tool by name. Never panics for tool-level problems: bad args and
/// unknown tools become error results (the model reads them and adapts).
pub async fn execute(name: &str, args: Value, ctx: &ToolContext) -> ToolResult {
    // The lookup scope must end before the await (a std MutexGuard held
    // across an await would make the future non-Send).
    let tool = {
        let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        match reg.tools.get(name) {
            Some(t) => t.clone(),
            None => {
                return ToolResult::err(format!("error: unknown tool {name:?}"));
            }
        }
    };
    let args = if args.is_null() { json!({}) } else { args };
    (tool.fn_)(args, ctx.clone()).await
}

/// Per-call context handed to every tool. Built at the dispatch site
/// (`AgentLoop`), so tools never reach for global process state.
#[derive(Clone)]
pub struct ToolContext {
    pub cfg: Config,
    pub workspace: PathBuf,
}

impl ToolContext {
    pub fn limits(&self) -> &crate::config::LimitsConfig {
        &self.cfg.tools.limits
    }

    /// Resolve a workspace-relative path; None when it escapes the workspace.
    pub fn safe(&self, raw: &str) -> Option<PathBuf> {
        safe_path(&self.workspace, raw)
    }

    pub fn jail_error(&self, raw: &str) -> ToolResult {
        ToolResult::err(format!(
            "error: {} escapes the workspace",
            if raw.is_empty() { "(empty path)" } else { raw }
        ))
    }
}

/// Collapse `.` / `..` textually (the lexical half of `Path::resolve`).
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // `..` at the top stays put; the final containment check
                // rejects whatever that produces.
                if out
                    .components()
                    .next()
                    .is_none_or(|c| matches!(c, std::path::Component::ParentDir))
                {
                    out.push("..");
                } else {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a workspace-relative path. None when it escapes the root.
///
/// Mirrors Python's `(root / raw).resolve()`: existing parts are
/// symlink-resolved, `..` is collapsed, and the candidate must be the root
/// or inside it. A symlink pointing outside resolves outside -> rejected.
pub fn safe_path(root: &Path, raw: &str) -> Option<PathBuf> {
    if raw.is_empty() {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let full = normalize_lexical(&root.join(raw));
    let resolved = if full.exists() {
        full.canonicalize().ok()?
    } else {
        // Target may not exist yet (writes): canonicalize the nearest
        // existing ancestor and re-attach the remaining components.
        let mut ancestor = full.clone();
        while !ancestor.exists() {
            let Some(parent) = ancestor.parent() else {
                return None;
            };
            ancestor = parent.to_path_buf();
        }
        let uncan_count = ancestor.components().count();
        let base = ancestor.canonicalize().ok()?;
        let mut out = base;
        for c in full.components().skip(uncan_count) {
            out = out.join(c.as_os_str());
        }
        out
    };
    (resolved == root || resolved.starts_with(&root)).then_some(resolved)
}

/// Truncate to `limit` chars, preferring a line boundary.
pub fn cap(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let cut: String = text.chars().take(limit).collect();
    let newline = cut.rfind('\n').filter(|&n| n > limit / 2);
    let cut = match newline {
        Some(n) => cut[..n].to_string(),
        None => cut,
    };
    format!("{cut}\n...[truncated at {limit} chars]")
}

/// Text read that tolerates bad UTF-8 (Python: errors="replace").
pub fn read_text(path: &Path) -> String {
    std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("harness-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn safe_path_jails_escape_attempts() {
        let root = tmp("jail");
        assert_eq!(safe_path(&root, "."), Some(root.canonicalize().unwrap()));
        let outside = root.parent().unwrap().join(format!("{}.secret", root.file_name().unwrap().to_string_lossy()));
        assert!(safe_path(&root, "../escape.txt").is_none());
        // absolute path pointing outside
        assert!(safe_path(&root, "/etc/passwd").is_none());
        // nested new path (does not exist yet) resolves inside
        assert!(safe_path(&root, "a/b/c.txt").is_some());
        // sneaky mix: inside then out then in
        assert!(safe_path(&root, "a/../../escape.txt").is_none());
        assert!(safe_path(&root, "").is_none());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn schemas_respect_approval_and_enabled() {
        crate::tools::register_all();
        let names: Vec<String> = enabled_names(&Config::default())
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(names.iter().any(|n| n == "read_file"));
        assert!(names.iter().any(|n| n == "run_command"));
        // tools off -> empty
        let mut off = Config::default();
        off.tools.enabled = false;
        assert!(enabled_names(&off).is_empty());
        // approval off -> hidden from the model
        let mut one = Config::default();
        one.tools.approval.insert("read_file".into(), "off".into());
        let hidden: Vec<String> = enabled_names(&one)
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(!hidden.iter().any(|n| n == "read_file"));
        // schema shape
        let schemas = schemas(&Config::default());
        assert!(
            schemas.iter().all(|s| {
                s["type"] == "function"
                    && s["function"]["parameters"]["type"] == "object"
            }),
            "bad schema shape: {schemas:?}"
        );
    }

    #[test]
    fn cap_truncates_at_line_boundary_when_sane() {
        let text = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let c = cap(&text, 60);
        assert!(c.ends_with("\n...[truncated at 60 chars]"));
        // cut should end just before a newline when one exists past half
        assert!(!c.trim_end().ends_with(' '));
        // tiny limit: no boundary available, raw cut
        let c2 = cap("abcdef", 4);
        assert!(c2.starts_with("abcd"));
        // under the limit: unchanged
        assert_eq!(cap("short", 100), "short");
    }
}
