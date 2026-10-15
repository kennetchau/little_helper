//! System-prompt composition (plan §4).
//!
//! `system_prompt = base_prompt(agent) + agent_folder_section()`. The agent
//! folder is the agent's WORKING space (scratch files, progress notes), so
//! NOT everything in it is injected — only the two stable cross-session
//! files at its top level, matched case-insensitively:
//!
//! - `AGENT.md` — session continuity (who the agent is, ground rules)
//! - `SKILL.md` — workspace-specific instructions
//!
//! The folder is probed at the top level of the workspace, matched
//! case-insensitively (`agent/`, `Agent/`, `AGENT/`). Each of the two files
//! is text-only (NUL bytes in the first 8 KB -> skipped), capped at
//! 20,000 chars each, section total capped at 50,000 chars. Any other file
//! in the folder (TODO.md, temp files, ...) is never injected: volatile
//! state must stay out of the fixed system prompt and be read on demand.
//!
//! This is called at session start (the prompt becomes `memory[0]`); the
//! effective prompt is recorded in the session log by the caller so a resume
//! reconstructs exactly what was sent.

use std::path::Path;

pub const DEFAULT_AGENT: &str = "coder";

/// Per-file and per-section caps (plan §4.1).
const FILE_CAP: usize = 20_000;
const SECTION_CAP: usize = 50_000;
const NUL_SCAN: usize = 8 * 1024;

/// The two files injected from the agent folder (case-insensitive).
const INJECTED: &[&str] = &["AGENT.md", "SKILL.md"];

const CODER_PROMPT: &str = "\
You are coder, a software engineering agent running in a Linux container. All \
file paths are relative to your workspace (the current directory).

Working style:
- Inspect before you modify: read/grep before write/edit.
- Make minimal, correct changes; verify your work when it is cheap to do so \
(run the program or a relevant test).
- If a tool call is denied by the user, do not retry the identical call. \
Adjust the approach or explain what you need instead.
- Follow existing project conventions; do not add files or dependencies \
that were not asked for.
- Keep the final reply short: what you did, plus any follow-ups.";

/// The base prompt for an agent (all agents share the coder body today).
pub fn base_prompt(agent: &str) -> String {
    match agent {
        DEFAULT_AGENT => CODER_PROMPT.to_string(),
        other => format!("You are {other}, a software engineering agent running in a Linux container. All file paths are relative to your workspace (the current directory)."),
    }
}

/// True when `bytes` looks like a non-text file (a NUL byte in the first
/// `NUL_SCAN` bytes) or is empty.
fn is_text(bytes: &[u8]) -> bool {
    !bytes.is_empty() && !bytes.iter().take(NUL_SCAN).any(|&b| b == 0)
}

/// Case-insensitive lookup of the agent folder at the workspace top level.
/// An exact `agent` wins if multiple case variants coexist.
fn find_agent_folder(workspace: &Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(workspace).ok()?;
    let mut fallback: Option<std::path::PathBuf> = None;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if name == "agent" {
            return Some(entry.path());
        }
        if name.eq_ignore_ascii_case("agent") {
            fallback = Some(entry.path());
        }
    }
    fallback
}

/// Case-insensitive lookup of a file inside `folder` for the canonical
/// name (e.g. `SKILL.md` matches `skill.md`); returns the on-disk path.
/// Directories, symlinks to non-files and broken links are skipped.
fn find_injected_file(folder: &Path, canonical: &str) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(folder).ok()?;
    let mut fallback: Option<std::path::PathBuf> = None;
    for entry in entries.flatten() {
        // `metadata` follows symlinks: a directory or broken link is
        // skipped, a link to a regular file is kept.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if name == canonical {
            return Some(entry.path());
        }
        if name.eq_ignore_ascii_case(canonical) {
            fallback = Some(entry.path());
        }
    }
    fallback
}

/// The `## Agent notes` subsection, or None when the folder is absent or
/// none of the injected files is a loadable text file.
pub fn agent_folder_section(workspace: &Path) -> Option<String> {
    let folder = find_agent_folder(workspace)?;
    let mut section = String::new();
    let mut added = 0usize;
    for name in INJECTED {
        let Some(path) = find_injected_file(&folder, name) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if !is_text(&bytes) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let text: String = text.chars().take(FILE_CAP).collect();

        let block = format!("\n### {name}\n{text}");
        if section.chars().count() + block.chars().count() > SECTION_CAP {
            // Section cap hit: keep whatever fits; stop (do not clip a file
            // mid-block, matching Python's section-total cap behavior).
            break;
        }
        section.push_str(&block);
        added += 1;
    }

    if added == 0 {
        return None;
    }
    Some(format!(
        "\n\n## Agent notes\n\nThe workspace contains an agent folder. Follow its contents:{section}"
    ))
}

/// Compose the full system prompt: base + agent notes.
pub fn system_prompt(agent: &str, workspace: &Path) -> String {
    let base = base_prompt(agent);
    match agent_folder_section(workspace) {
        Some(sec) => format!("{base}{sec}"),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_prompt_is_stable() {
        assert!(base_prompt("coder").starts_with("You are coder,"));
        assert!(base_prompt("coder").contains("Inspect before you modify"));
        // unknown agent: a generic body, still present
        assert!(base_prompt("writer").starts_with("You are writer,"));
    }

    #[test]
    fn agent_folder_absent_yields_no_section() {
        let dir = std::env::temp_dir().join(format!("harness-prompt-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(agent_folder_section(&dir).is_none());
        assert_eq!(system_prompt("coder", &dir), base_prompt("coder"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_skill_and_agent_md_are_injected() {
        for case in ["agent", "Agent", "AGENT"] {
            let dir = std::env::temp_dir().join(format!("harness-prompt-{case}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let folder = dir.join(case);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("AGENT.md"), "continuity").unwrap();
            std::fs::write(folder.join("skill.md"), "workspace skill").unwrap();
            // everything else is scratch: never injected
            std::fs::write(folder.join("TODO.md"), "progress todo").unwrap();
            std::fs::write(folder.join("notes.txt"), "scratch notes").unwrap();
            let section = agent_folder_section(&dir).expect("section");
            assert!(section.contains("## Agent notes"));
            assert!(section.contains("Follow its contents"));
            // canonical header names even when the disk case differs
            assert!(section.contains("### AGENT.md"));
            assert!(section.contains("### SKILL.md"));
            assert!(section.contains("continuity") && section.contains("workspace skill"));
            assert!(!section.contains("### TODO.md"));
            assert!(!section.contains("### notes.txt"));
            assert!(!section.contains("progress todo"));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn non_text_injected_file_skipped() {
        let dir = std::env::temp_dir().join(format!("harness-prompt-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let folder = dir.join("agent");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("SKILL.md"), "fine").unwrap();
        std::fs::write(folder.join("AGENT.md"), b"\x00\x01\x02").unwrap();
        let section = agent_folder_section(&dir).expect("text file kept");
        assert!(section.contains("### SKILL.md"));
        assert!(!section.contains("### AGENT.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_file_cap_applies() {
        let dir = std::env::temp_dir().join(format!("harness-prompt-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let folder = dir.join("agent");
        std::fs::create_dir_all(&folder).unwrap();
        let big = "x".repeat(FILE_CAP + 500);
        std::fs::write(folder.join("SKILL.md"), &big).unwrap();
        let section = agent_folder_section(&dir).expect("section");
        // file content clipped to FILE_CAP
        assert!(section.chars().count() < big.chars().count() + 4096);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
