//! File tools: list_dir, read_file, write_file, edit_file, delete_file,
//! grep, glob, git_diff. All paths are workspace-relative and jailed.

use std::io::Read;
use std::path::Path;

use regex::Regex;
use serde_json::{json, Value};

use super::registry::{read_text, cap, register, ToolContext, ToolResult};

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Image detection for read_file's vision support (CR_01/8): the extension
/// picks the data-URI mime, and the first magic bytes are a cheap sanity
/// check so a misnamed text file is not base64-blobbed into the model.
fn image_mime(p: &Path) -> Option<&'static str> {
    let ext = p.extension()?.to_string_lossy().to_ascii_lowercase();
    let mut magic = [0u8; 12];
    let n = std::fs::File::open(p).ok()?.read(&mut magic).ok()?;
    let m = &magic[..n];
    let magic_ok = match ext.as_str() {
        "png" => m.len() >= 8
            && m[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        "jpg" | "jpeg" => m.len() >= 3 && m[0] == 0xFF && m[1] == 0xD8 && m[2] == 0xFF,
        "gif" => m.len() >= 6
            && (m[..6] == [b'G', b'I', b'F', b'8', b'7', b'a']
                || m[..6] == [b'G', b'I', b'F', b'8', b'9', b'a']),
        "webp" => m.len() >= 12
            && m[0..4] == [0x52, 0x49, 0x46, 0x46]
            && m[8..12] == [0x57, 0x45, 0x42, 0x50],
        "bmp" => m.len() >= 2 && m[0] == b'B' && m[1] == b'M',
        "tiff" | "tif" => m.len() >= 4
            && (m[..4] == [0x49, 0x49, 0x2A, 0x00] || m[..4] == [0x4D, 0x4D, 0x00, 0x2A]),
        // svg is a text format: fall through to the text reader
        _ => false,
    };
    if !magic_ok {
        return None;
    }
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tiff" | "tif" => "image/tiff",
        _ => unreachable!(),
    })
}

fn u64_arg(args: &Value, key: &str, default: u64) -> u64 {
    args.get(key).and_then(Value::as_u64).unwrap_or(default)
}

fn _list_dir(args: Value, ctx: &ToolContext) -> ToolResult {
    let raw = str_arg(&args, "path").unwrap_or(".").to_string();
    let Some(p) = ctx.safe(&raw) else {
        return ctx.jail_error(&raw);
    };
    if !p.is_dir() {
        return ToolResult::err(format!("error: {raw}: not a directory"));
    }
    let mut entries: Vec<(bool, String)> = match std::fs::read_dir(&p) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                (is_dir, e.file_name().to_string_lossy().into_owned())
            })
            .collect(),
        Err(e) => return ToolResult::err(format!("error: {raw}: {e}")),
    };
    // Python: sorted by (is_file, name.lower()) — files first, casefolded.
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));
    let lines: Vec<String> = entries
        .into_iter()
        .map(|(is_dir, name)| format!("{name}{}", if is_dir { "/" } else { "" }))
        .collect();
    ToolResult::res(cap(&lines.join("\n"), ctx.limits().exec_output_chars))
}

fn _read_file(args: Value, ctx: &ToolContext) -> ToolResult {
    let Some(raw) = str_arg(&args, "path") else {
        return ToolResult::err("error: path is required".to_string());
    };
    let Some(p) = ctx.safe(raw) else {
        return ctx.jail_error(raw);
    };
    if !p.is_file() {
        return ToolResult::err(format!("error: {raw}: no such file"));
    }
    // Vision (CR_01/8): images are returned to the model as base64
    // data-URI parts instead of a "binary file" error (the local
    // llama.cpp server accepts image_url with a base64 data URI).
    if let Some(mime) = image_mime(&p) {
        let bytes = match std::fs::read(&p) {
            Ok(b) => b,
            Err(e) => return ToolResult::err(format!("error: {raw}: {e}")),
        };
        let text = format!(
            "{raw} is an image ({mime}, {} bytes). The image is included with this result so you can look at it directly.",
            bytes.len()
        );
        let uri = format!(
            "data:{};base64,{}",
            mime,
            base64::engine::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes)
        );
        let mut r = ToolResult::res(text);
        r.images.push(uri);
        return r;
    }
    let text = read_text(&p);
    if text.chars().take(8192).any(|c| c == '\0') {
        return ToolResult::err(format!(
            "error: {raw}: binary file, not readable as text (and not a recognized image)"
        ));
    }
    let lines: Vec<String> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    let total = lines.len();
    let offset = u64_arg(&args, "offset", 1).max(1) as usize;
    let limit = (u64_arg(&args, "limit", ctx.limits().max_read_lines as u64)
        .min(ctx.limits().max_read_lines as u64)) as usize;
    if offset > total {
        return ToolResult::err(format!(
            "error: {raw}: offset {offset} is past end of file ({total} lines)"
        ));
    }
    let chunk: Vec<&str> = lines.iter().skip(offset - 1).take(limit).map(String::as_str).collect();
    let header = format!("# {raw} lines {}-{} of {total}", offset, offset + chunk.len() - 1);
    ToolResult::res(cap(
        &format!("{header}\n{}", chunk.join("\n")),
        ctx.limits().exec_output_chars,
    ))
}

fn _write_file(args: Value, ctx: &ToolContext) -> ToolResult {
    let Some(raw) = str_arg(&args, "path") else {
        return ToolResult::err("error: path is required".to_string());
    };
    let Some(p) = ctx.safe(raw) else {
        return ctx.jail_error(raw);
    };
    let Some(content) = str_arg(&args, "content") else {
        return ToolResult::err("error: content must be a string".to_string());
    };
    if p.exists() && !p.is_file() {
        return ToolResult::err(format!("error: {raw}: exists and is not a file"));
    }
    if let Some(parent) = p.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return ToolResult::err(format!("error: {raw}: cannot create parent dirs: {e}"));
        }
    }
    match std::fs::write(&p, content) {
        Ok(()) => ToolResult::res(format!("wrote {} chars to {raw}", content.chars().count())),
        Err(e) => ToolResult::err(format!("error: {raw}: {e}")),
    }
}

fn spill_new_string(ctx: &ToolContext, raw: &str, new: &str) -> Option<String> {
    // Write the model's replacement to a side file so it is not lost
    // when the edit fails (CR_01/7) — the model can recover it from
    // there instead of regenerating it. Best-effort; None on any error.
    let dir = ctx.workspace.join(".harness");
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    // name: <base-of-path>.edit-<n> (n bumps past existing files)
    let base = std::path::Path::new(raw)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "edit".to_string());
    let mut n = 1usize;
    let mut path = dir.join(format!("{base}.edit-{n}"));
    while path.exists() {
        n += 1;
        path = dir.join(format!("{base}.edit-{n}"));
    }
    let rel = path.strip_prefix(&ctx.workspace).ok()?.to_string_lossy().into_owned();
    std::fs::write(&path, new).ok()?;
    Some(rel)
}

fn _edit_file(args: Value, ctx: &ToolContext) -> ToolResult {
    let Some(raw) = str_arg(&args, "path") else {
        return ToolResult::err("error: path is required".to_string());
    };
    let Some(p) = ctx.safe(raw) else {
        return ctx.jail_error(raw);
    };
    if !p.is_file() {
        return ToolResult::err(format!("error: {raw}: no such file"));
    }
    let (Some(old), Some(new)) = (str_arg(&args, "old_string"), str_arg(&args, "new_string"))
    else {
        return ToolResult::err(
            "error: old_string and new_string must be strings".to_string(),
        );
    };
    let text = read_text(&p);
    let count = text.matches(old).count();
    if count == 0 {
        // CR_01/7: don't lose the model's replacement — spill it to a
        // side file it can recover, and say so.
        let spill = spill_new_string(ctx, raw, new)
            .map(|rel| format!(" (new_string saved to {rel})"))
            .unwrap_or_default();
        return ToolResult::err(format!(
            "error: {raw}: old_string not found (exact match required, including whitespace and indentation){spill}"
        ));
    }
    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if count > 1 && !replace_all {
        let spill = spill_new_string(ctx, raw, new)
            .map(|rel| format!(" (new_string saved to {rel})"))
            .unwrap_or_default();
        return ToolResult::err(format!(
            "error: {raw}: old_string matches {count} times; add surrounding context to make it unique or set replace_all=true{spill}"
        ));
    }
    let updated = if replace_all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    };
    match std::fs::write(&p, &updated) {
        Ok(()) => {
            let n = if replace_all { count } else { 1 };
            ToolResult::res(format!("edited {raw}: replaced {n} occurrence(s)"))
        }
        Err(e) => ToolResult::err(format!("error: {raw}: {e}")),
    }
}

fn _delete_file(args: Value, ctx: &ToolContext) -> ToolResult {
    let Some(raw) = str_arg(&args, "path") else {
        return ToolResult::err("error: path is required".to_string());
    };
    let Some(p) = ctx.safe(raw) else {
        return ctx.jail_error(raw);
    };
    if !p.is_file() {
        return ToolResult::err(format!(
            "error: {raw}: not a file (directories can't be deleted)"
        ));
    }
    match std::fs::remove_file(&p) {
        Ok(()) => ToolResult::res(format!("deleted {raw}")),
        Err(e) => ToolResult::err(format!("error: {raw}: {e}")),
    }
}

/// Sorted file iterator with an optional filename glob filter
/// (Python: fnmatch on the basename).
fn iter_files(base: &Path, include: Option<&str>) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if base.is_file() {
        if let Some(pat) = include {
            let name = base.file_name().map(|n| n.to_string_lossy().into_owned());
            if name.as_deref().map(|n| glob_match(pat, n)).unwrap_or(false) {
                out.push(base.to_path_buf());
            }
        } else {
            out.push(base.to_path_buf());
        }
        return out;
    }
    let mut dirs = vec![base.to_path_buf()];
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    while let Some(dir) = dirs.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        // sort entries so the final path list is deterministic (matches
        // Python's sorted rglob)
        let mut entries: Vec<std::path::PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                dirs.push(p);
            } else {
                if let Some(pat) = include {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
                    if !name.as_deref().map(|n| glob_match(pat, n)).unwrap_or(false) {
                        continue;
                    }
                }
                files.push(p);
            }
        }
    }
    files
}

/// fnmatch on the basename, case-insensitive like Python's fnmatch on
/// POSIX. `*`/`?`/literal, iterative backtracking.
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star_p, mut star_n) = (usize::MAX, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = pi;
            star_n = ni;
            pi += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_n += 1;
            ni = star_n;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn _grep(args: Value, ctx: &ToolContext) -> ToolResult {
    let Some(pattern) = str_arg(&args, "pattern") else {
        return ToolResult::err("error: pattern is required".to_string());
    };
    let Ok(rx) = Regex::new(pattern) else {
        return ToolResult::err(format!("error: invalid regex: {pattern:?}"));
    };
    let raw = str_arg(&args, "path").unwrap_or(".").to_string();
    let Some(base) = ctx.safe(&raw) else {
        return ctx.jail_error(&raw);
    };
    if !base.exists() {
        return ToolResult::err(format!("error: {raw}: no such path"));
    }
    let include = str_arg(&args, "include");
    let limit = ctx.limits().exec_output_chars;
    let mut hits: Vec<String> = Vec::new();
    for f in iter_files(&base, include) {
        let rel = f
            .strip_prefix(&ctx.workspace)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| f.to_string_lossy().into_owned());
        for (i0, line) in read_text(&f).split('\n').enumerate() {
            let i = i0 + 1;
            let line = line.strip_suffix('\r').unwrap_or(line);
            if rx.is_match(line) {
                hits.push(format!("{rel}:{i}: {line}"));
            }
        }
        // Python stops once the joined output exceeds the cap.
        if hits.join("\n").len() > limit {
            break;
        }
    }
    if hits.is_empty() {
        return ToolResult::res(format!("no matches for {pattern:?} in {}", if raw.is_empty() { "." } else { raw.as_str() }));
    }
    ToolResult::res(cap(&hits.join("\n"), limit))
}

fn _glob(args: Value, ctx: &ToolContext) -> ToolResult {
    let Some(pattern) = str_arg(&args, "pattern") else {
        return ToolResult::err("error: pattern is required".to_string());
    };
    let Ok(g) = glob::Pattern::new(pattern) else {
        return ToolResult::err(format!("error: invalid glob pattern: {pattern:?}"));
    };
    let mut matches: Vec<std::path::PathBuf> = Vec::new();
    let mut stack = vec![ctx.workspace.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                // don't descend into .git (Python glob follows it; the cost is
                // not worth it — but keep parity by still matching inside?
                // Python: Path.glob includes .git. Keep it simple: skip .git
                // to stay fast; models rarely glob inside it.)
                if p.file_name().map(|n| n == ".git").unwrap_or(false) {
                    continue;
                }
                stack.push(p);
            } else {
                let rel = match p.strip_prefix(&ctx.workspace) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                if g.matches_path(&rel) {
                    matches.push(p);
                }
            }
        }
    }
    matches.sort_by(|a, b| {
        let ra = a.strip_prefix(&ctx.workspace).unwrap_or(a);
        let rb = b.strip_prefix(&ctx.workspace).unwrap_or(b);
        ra.to_string_lossy().cmp(&rb.to_string_lossy())
    });
    if matches.is_empty() {
        return ToolResult::res(format!("no files match {pattern:?}"));
    }
    let lines: Vec<String> = matches
        .iter()
        .map(|p| {
            p.strip_prefix(&ctx.workspace)
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .collect();
    ToolResult::res(cap(&lines.join("\n"), ctx.limits().exec_output_chars))
}

fn _git_diff(_args: Value, ctx: &ToolContext) -> ToolResult {
    let out = std::process::Command::new("git")
        .args(["diff", "HEAD"])
        .current_dir(&ctx.workspace)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
            if stdout.trim().is_empty() {
                ToolResult::res("no changes vs HEAD")
            } else {
                ToolResult::res(cap(&stdout, ctx.limits().exec_output_chars))
            }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            ToolResult::err(format!("error: git diff failed: {stderr}"))
        }
        Err(e) => ToolResult::err(format!("error: git diff failed: {e}")),
    }
}

pub fn register_all() {
    register(
        "list_dir",
        "List directory entries (directories marked with a trailing /).",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string",
                         "description": "Directory relative to the workspace root (default '.')"}
            }
        }),
        |args, ctx| Box::pin(async move { _list_dir(args, &ctx) }),
    );
    register(
        "read_file",
        "Read a text file, optionally a line range. Image files (png/jpg/gif/webp/bmp/tiff) are returned as an image you can look at directly (vision).",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File relative to the workspace root"},
                "offset": {"type": "integer", "minimum": 1,
                           "description": "First line to read, 1-based (default 1)"},
                "limit": {"type": "integer", "minimum": 1,
                          "description": "Max lines to read (default: configured max_read_lines)"}
            },
            "required": ["path"]
        }),
        |args, ctx| Box::pin(async move { _read_file(args, &ctx) }),
    );
    register(
        "write_file",
        "Create or overwrite a file with the given content. Parent dirs are created.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File relative to the workspace root"},
                "content": {"type": "string", "description": "Full file content"}
            },
            "required": ["path", "content"]
        }),
        |args, ctx| Box::pin(async move { _write_file(args, &ctx) }),
    );
    register(
        "edit_file",
        "Replace an exact string in a file. Fails unless old_string matches exactly once, unless replace_all is true. If the edit fails, the new_string you sent is saved to a side file under .harness/ (the error says where) so you can recover it with read_file instead of regenerating it.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File relative to the workspace root"},
                "old_string": {"type": "string", "description": "Exact text to find"},
                "new_string": {"type": "string", "description": "Replacement text"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
            },
            "required": ["path", "old_string", "new_string"]
        }),
        |args, ctx| Box::pin(async move { _edit_file(args, &ctx) }),
    );
    register(
        "delete_file",
        "Delete a file.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File relative to the workspace root"}
            },
            "required": ["path"]
        }),
        |args, ctx| Box::pin(async move { _delete_file(args, &ctx) }),
    );
    register(
        "grep",
        "Search files for a regular expression. Returns 'path:line: text' hits.",
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression"},
                "path": {"type": "string", "description": "File or directory to search (default '.')"},
                "include": {"type": "string", "description": "Filename glob filter, e.g. '*.py' (default: all)"}
            },
            "required": ["pattern"]
        }),
        |args, ctx| Box::pin(async move { _grep(args, &ctx) }),
    );
    register(
        "glob",
        "List files matching a glob pattern, relative to the workspace root.",
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern, e.g. 'src/**/*.py'"}
            },
            "required": ["pattern"]
        }),
        |args, ctx| Box::pin(async move { _glob(args, &ctx) }),
    );
    register(
        "git_diff",
        "Show the unified diff of the workspace against HEAD (staged + unstaged).",
        json!({"type": "object", "properties": {}}),
        |args, ctx| Box::pin(async move { _git_diff(args, &ctx) }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LimitsConfig, WorkspaceConfig, ToolsConfig};
    use std::path::PathBuf;

    fn setup(name: &str) -> (PathBuf, ToolContext) {
        let ws = std::env::temp_dir().join(format!("harness-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        let cfg = Config {
            workspace: WorkspaceConfig {
                path: ws.to_string_lossy().into(),
                auto_git: true,
            },
            tools: ToolsConfig {
                limits: LimitsConfig {
                    max_read_lines: 100,
                    exec_output_chars: 500,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let ctx = ToolContext {
            cfg,
            workspace: ws.canonicalize().unwrap(),
        };
        (ws, ctx)
    }

    fn write(ctx: &ToolContext, path: &str, content: &str) {
        _write_file(
            json!({"path": path, "content": content}),
            ctx,
        )
        .ok
            .then_some(())
            .expect("write ok");
    }

    #[test]
    fn jail_rejects_escapes() {
        let (_ws, ctx) = setup("jail");
        let r = _read_file(json!({"path": "../outside.txt"}), &ctx);
        assert!(!r.ok);
        assert!(r.result.contains("escapes the workspace"));
        let r = _read_file(json!({"path": "/etc/passwd"}), &ctx);
        assert!(!r.ok && r.result.contains("escapes the workspace"));
        let r = _read_file(json!({"path": "a/../../etc/passwd"}), &ctx);
        assert!(!r.ok && r.result.contains("escapes the workspace"));
        let r = _read_file(json!({"path": ""}), &ctx);
        assert!(!r.ok && r.result.contains("escapes the workspace"));
    }

    #[test]
    fn list_read_write_roundtrip() {
        let (_ws, ctx) = setup("lrw");
        write(&ctx, "sub/a.txt", "hello\nworld");
        write(&ctx, "sub/b.txt", "x");
        write(&ctx, "top.txt", "t");
        let r = _list_dir(json!({"path": "sub"}), &ctx);
        assert!(r.ok);
        assert_eq!(r.result, "a.txt\nb.txt");
        let r = _list_dir(json!({}), &ctx);
        assert!(r.ok);
        assert_eq!(r.result, "sub/\ntop.txt"); // dirs first
        let r = _read_file(json!({"path": "sub/a.txt"}), &ctx);
        assert!(r.ok);
        assert!(r.result.contains("# sub/a.txt lines 1-2 of 2"));
        assert!(r.result.contains("hello\nworld"));
        let r = _read_file(json!({"path": "sub/a.txt", "offset": 2, "limit": 1}), &ctx);
        assert!(r.ok);
        assert!(r.result.contains("lines 2-2 of 2"));
        assert!(r.result.ends_with("world"));
        let r = _read_file(json!({"path": "sub/a.txt", "offset": 99}), &ctx);
        assert!(!r.ok && r.result.contains("past end of file"));
        let r = _list_dir(json!({"path": "nope"}), &ctx);
        assert!(!r.ok && r.result.contains("not a directory"));
    }

    #[test]
    fn edit_uniqueness_and_replace_all() {
        let (_ws, ctx) = setup("edit");
        write(&ctx, "e.txt", "a\nb\na\nb");
        // unique
        let r = _edit_file(json!({"path": "e.txt", "old_string": "a", "new_string": "A"}), &ctx);
        assert!(!r.ok && r.result.contains("matches 2 times"));
        // replace_all
        let r = _edit_file(
            json!({"path": "e.txt", "old_string": "a", "new_string": "A", "replace_all": true}),
            &ctx,
        );
        assert!(r.ok && r.result.contains("replaced 2 occurrence(s)"));
        assert_eq!(std::fs::read_to_string(ctx.workspace.join("e.txt")).unwrap(), "A\nb\nA\nb");
        // not found
        let r = _edit_file(json!({"path": "e.txt", "old_string": "zz", "new_string": "q"}), &ctx);
        assert!(!r.ok && r.result.contains("not found"));
        // missing file
        let r = _edit_file(json!({"path": "no.txt", "old_string": "x", "new_string": "y"}), &ctx);
        assert!(!r.ok && r.result.contains("no such file"));
    }

    #[test]
    fn delete_semantics() {
        let (_ws, ctx) = setup("del");
        write(&ctx, "d.txt", "x");
        assert!(_delete_file(json!({"path": "d.txt"}), &ctx).ok);
        assert!(!_delete_file(json!({"path": "d.txt"}), &ctx).ok);
        let r = _delete_file(json!({"path": "sub2"}), &ctx);
        assert!(!r.ok && r.result.contains("not a file"));
    }

    #[test]
    fn grep_finds_and_filters() {
        let (_ws, ctx) = setup("grep");
        write(&ctx, "src/a.py", "def foo():\n    pass\n");
        write(&ctx, "src/b.rs", "fn main() {}\nfn foo() {}\n");
        write(&ctx, "c.txt", "nothing\n");
        let r = _grep(json!({"pattern": "foo"}), &ctx);
        assert!(r.ok);
        assert!(r.result.contains("src/a.py:1:"), "{}", r.result);
        assert!(!r.result.contains("src/b.rs:1:"), "main line has no foo: {}", r.result);
        assert!(r.result.contains("src/b.rs:2:"), "{}", r.result);
        assert!(!r.result.contains("c.txt"));
        let r = _grep(json!({"pattern": "foo", "include": "*.py"}), &ctx);
        assert!(r.ok);
        assert!(r.result.contains("src/a.py:1:"));
        assert!(!r.result.contains("src/b.rs"));
        let r = _grep(json!({"pattern": "[invalid"}), &ctx);
        assert!(!r.ok && r.result.contains("invalid regex"));
        let r = _grep(json!({"pattern": "definitely_nothing"}), &ctx);
        assert!(r.ok && r.result.contains("no matches"));
    }

    #[test]
    fn glob_matches() {
        let (_ws, ctx) = setup("glob");
        write(&ctx, "src/deep/a.rs", "x");
        write(&ctx, "src/b.rs", "x");
        write(&ctx, "top.txt", "x");
        let r = _glob(json!({"pattern": "src/**/*.rs"}), &ctx);
        assert!(r.ok);
        assert_eq!(r.result, "src/b.rs\nsrc/deep/a.rs");
        let r = _glob(json!({"pattern": "*.txt"}), &ctx);
        assert_eq!(r.result, "top.txt");
        let r = _glob(json!({"pattern": "nope/**"}), &ctx);
        assert!(r.ok && r.result.contains("no files match"));
    }

    #[test]
    fn read_binary_rejected() {
        let (ws, ctx) = setup("bin");
        std::fs::write(ws.join("bin.dat"), [0u8, 1, 0, 2]).unwrap();
        let r = _read_file(json!({"path": "bin.dat"}), &ctx);
        assert!(!r.ok && r.result.contains("binary file"));
    }

    #[test]
    fn read_image_returns_vision_parts() {
        let (ws, ctx) = setup("vision");
        // minimal 1x1 PNG (valid magic + IHDR)
        let png: &[u8] = &[
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13,
            0x49, 0x48, 0x44, 0x52,
        ];
        std::fs::write(ws.join("pix.png"), png).unwrap();
        let r = _read_file(json!({"path": "pix.png"}), &ctx);
        assert!(r.ok, "{}", r.result);
        assert!(r.result.contains("image/png"), "{}", r.result);
        assert_eq!(r.images.len(), 1);
        assert!(r.images[0].starts_with("data:image/png;base64,"));
        // decode back to the exact bytes
        let b64 = r.images[0].split_once("base64,").unwrap().1;
        let decoded = base64::engine::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            b64,
        )
        .unwrap();
        assert_eq!(decoded, png);
    }

    #[test]
    fn misnamed_image_falls_back_to_binary() {
        let (ws, ctx) = setup("misnamed");
        // .png extension but text contents -> magic mismatch -> text path
        // (and since it has no NUL, it reads as text, not "binary").
        std::fs::write(ws.join("fake.png"), "hello\nworld\n").unwrap();
        let r = _read_file(json!({"path": "fake.png"}), &ctx);
        assert!(r.ok, "{}", r.result);
        assert!(r.images.is_empty());
        assert!(r.result.contains("hello\nworld"), "{}", r.result);
    }

    #[test]
    fn edit_failure_spills_new_string() {
        let (ws, ctx) = setup("spill");
        write(&ctx, "f.txt", "hello\n");
        // not found -> error + spill
        let r = _edit_file(
            json!({"path": "f.txt", "old_string": "nope", "new_string": "THE NEW TEXT"}),
            &ctx,
        );
        assert!(!r.ok && r.result.contains("not found"), "{}", r.result);
        // the spill file is named in the error and exists with the content
        let Some(rel) = r.result.rsplit("saved to ").nth(0) else {
            panic!("no spill path in error: {}", r.result);
        };
        let spill_path = ws.join(rel.split(')').next().unwrap().trim());
        assert!(spill_path.exists(), "spill missing: {}", spill_path.display());
        assert_eq!(
            std::fs::read_to_string(&spill_path).unwrap(),
            "THE NEW TEXT"
        );
        // the target file is untouched
        assert_eq!(std::fs::read_to_string(ws.join("f.txt")).unwrap(), "hello\n");
        // repeated failures bump the suffix (no overwrite of a spill)
        let r2 = _edit_file(
            json!({"path": "f.txt", "old_string": "nope", "new_string": "AGAIN"}),
            &ctx,
        );
        assert!(!r2.ok);
        let rel2 = r2.result.rsplit("saved to ").nth(0).unwrap().split(')').next().unwrap().trim();
        assert_ne!(rel, rel2, "second spill should use a new name: {rel2}");
    }
}
