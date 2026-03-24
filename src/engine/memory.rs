use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::storage::{ChatMessage, Session};
use crate::util::{
    DEFAULT_HISTORY_TEMPLATE, DEFAULT_MEMORY_TEMPLATE, ensure_dir, estimate_json_tokens, now_iso,
    workspace_state_dir,
};

const MEMORY_ENTRIES_HEADING: &str = "## Memory Entries";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryEntryKind {
    TaskSummary,
    UserInstructed,
}

impl MemoryEntryKind {
    fn label(self) -> &'static str {
        match self {
            Self::TaskSummary => "Task Summary",
            Self::UserInstructed => "User Instructed Memory",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub kind: MemoryEntryKind,
    pub title: String,
    pub summary: String,
    pub attention_points: Vec<String>,
    pub recorded_at: String,
}

impl MemoryEntry {
    pub fn to_markdown(&self) -> String {
        let mut lines = vec![
            format!("### {}", self.title.trim()),
            format!("- Type: {}", self.kind.label()),
            format!("- Time: {}", self.recorded_at.trim()),
            format!("- Summary: {}", self.summary.trim()),
        ];
        if self.attention_points.is_empty() {
            lines.push("- Attention: none recorded".to_string());
        } else {
            lines.push("- Attention:".to_string());
            for point in &self.attention_points {
                lines.push(format!("  - {}", point.trim()));
            }
        }
        lines.join("\n")
    }
}

pub struct MemoryStore {
    memory_dir: PathBuf,
    memory_file: PathBuf,
    history_file: PathBuf,
    max_memory_bytes: usize,
}

impl MemoryStore {
    pub fn new(workspace: &Path, max_memory_bytes: usize) -> Result<Self> {
        let memory_dir = ensure_dir(workspace_state_dir(workspace).join("memory"))?;
        Ok(Self {
            memory_file: memory_dir.join("MEMORY.md"),
            history_file: memory_dir.join("HISTORY.md"),
            memory_dir,
            max_memory_bytes: max_memory_bytes.max(1),
        })
    }

    pub fn read_long_term(&self) -> Result<String> {
        Ok(if self.memory_file.exists() {
            fs::read_to_string(&self.memory_file)?
        } else {
            DEFAULT_MEMORY_TEMPLATE.to_string()
        })
    }

    pub fn write_long_term(&self, content: &str) -> Result<()> {
        let (preface, mut entries) = split_memory_document(content);
        let rendered = render_memory_document(&preface, &mut entries, self.max_memory_bytes);
        fs::write(&self.memory_file, rendered)?;
        Ok(())
    }

    pub fn append_memory_entry(&self, entry: &MemoryEntry) -> Result<()> {
        let current = self.read_long_term()?;
        let (preface, mut entries) = split_memory_document(&current);
        entries.push(entry.to_markdown());
        let rendered = render_memory_document(&preface, &mut entries, self.max_memory_bytes);
        fs::write(&self.memory_file, rendered)?;
        Ok(())
    }

    pub fn append_history(&self, entry: &str) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_file)?;
        writeln!(file, "{}\n", entry.trim_end())?;
        Ok(())
    }

    pub fn reset_history(&self) -> Result<()> {
        fs::write(&self.history_file, DEFAULT_HISTORY_TEMPLATE)?;
        Ok(())
    }

    pub fn get_memory_context(&self, topic: &str) -> Result<String> {
        let current = self.read_long_term()?;
        let (preface, entries) = split_memory_document(&current);
        let preface = extract_preface_context(&preface);
        let relevant_entries = select_relevant_entries(topic, &entries, self.max_memory_bytes / 4);

        let mut parts = Vec::new();
        if !preface.trim().is_empty() {
            parts.push(preface);
        }
        if !relevant_entries.is_empty() {
            parts.push(format!(
                "## Relevant Memory Entries\n\n{}",
                relevant_entries.join("\n\n")
            ));
        }

        Ok(if parts.is_empty() {
            String::new()
        } else {
            format!("## Long-term Memory\n{}", parts.join("\n\n"))
        })
    }

    pub fn archive_raw_messages(&self, messages: &[ChatMessage]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut lines = Vec::new();
        lines.push(format!(
            "[{}] [RAW] {} messages",
            now_iso().chars().take(16).collect::<String>(),
            messages.len()
        ));
        for message in messages {
            if let Some(text) = message.content_as_text() {
                lines.push(format!(
                    "[{}] {}: {}",
                    message
                        .timestamp
                        .as_deref()
                        .unwrap_or("?")
                        .chars()
                        .take(16)
                        .collect::<String>(),
                    message.role.to_uppercase(),
                    text
                ));
            }
        }
        self.append_history(&lines.join("\n"))
    }

    pub fn memory_dir(&self) -> &Path {
        &self.memory_dir
    }
}

pub struct MemoryConsolidator {
    store: MemoryStore,
    context_window_tokens: usize,
}

impl MemoryConsolidator {
    pub fn new(
        workspace: &Path,
        context_window_tokens: usize,
        max_memory_bytes: usize,
    ) -> Result<Self> {
        Ok(Self {
            store: MemoryStore::new(workspace, max_memory_bytes)?,
            context_window_tokens,
        })
    }

    pub fn store(&self) -> &MemoryStore {
        &self.store
    }

    pub fn estimate_session_prompt_tokens(&self, session: &Session) -> usize {
        session
            .get_history(0)
            .iter()
            .map(|message| {
                let mut total = 4;
                if let Some(content) = &message.content {
                    total += estimate_json_tokens(content);
                }
                if let Some(tool_calls) = &message.tool_calls {
                    total += tool_calls.iter().map(estimate_json_tokens).sum::<usize>();
                }
                total
            })
            .sum()
    }

    pub fn maybe_consolidate_by_tokens(&self, session: &mut Session) -> Result<()> {
        if self.context_window_tokens == 0 {
            return Ok(());
        }
        let target = (self.context_window_tokens * 3) / 4;
        while self.estimate_session_prompt_tokens(session) > target
            && session.last_consolidated < session.messages.len()
        {
            let remaining = &session.messages[session.last_consolidated..];
            let boundary = remaining
                .iter()
                .position(|message| message.role == "user" && session.last_consolidated > 0)
                .unwrap_or(remaining.len().min(8))
                .max(1);
            let end = (session.last_consolidated + boundary).min(session.messages.len());
            let chunk = &session.messages[session.last_consolidated..end];
            self.store.archive_raw_messages(chunk)?;
            session.last_consolidated = end;
        }
        Ok(())
    }

    pub fn archive_messages(&self, messages: &[ChatMessage]) -> Result<()> {
        self.store.archive_raw_messages(messages)
    }
}

fn split_memory_document(content: &str) -> (String, Vec<String>) {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return (default_memory_preface(), Vec::new());
    }

    let Some((preface, rest)) = trimmed.split_once(MEMORY_ENTRIES_HEADING) else {
        return (trimmed.to_string(), Vec::new());
    };

    let entries = rest
        .split("\n### ")
        .filter_map(|block| {
            let block = block.trim();
            if block.is_empty() {
                return None;
            }
            Some(if block.starts_with("### ") {
                block.to_string()
            } else {
                format!("### {block}")
            })
        })
        .collect();
    (preface.trim_end().to_string(), entries)
}

fn render_memory_document(
    preface: &str,
    entries: &mut Vec<String>,
    max_memory_bytes: usize,
) -> String {
    let preface = if preface.trim().is_empty() {
        default_memory_preface()
    } else {
        preface.trim_end().to_string()
    };

    loop {
        let mut rendered = format!("{preface}\n\n{MEMORY_ENTRIES_HEADING}");
        if !entries.is_empty() {
            rendered.push_str("\n\n");
            rendered.push_str(&entries.join("\n\n"));
        }
        rendered.push('\n');

        if rendered.len() <= max_memory_bytes || entries.is_empty() {
            if rendered.len() <= max_memory_bytes {
                return rendered;
            }
            return trim_to_last_bytes(&rendered, max_memory_bytes);
        }

        entries.remove(0);
    }
}

fn default_memory_preface() -> String {
    DEFAULT_MEMORY_TEMPLATE
        .split_once(MEMORY_ENTRIES_HEADING)
        .map(|(preface, _)| preface.trim_end().to_string())
        .unwrap_or_else(|| DEFAULT_MEMORY_TEMPLATE.trim_end().to_string())
}

fn trim_to_last_bytes(content: &str, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content.to_string();
    }

    let mut start = content.len().saturating_sub(max_bytes);
    while !content.is_char_boundary(start) && start < content.len() {
        start += 1;
    }
    content[start..].to_string()
}

fn extract_preface_context(preface: &str) -> String {
    let mut lines = Vec::new();
    for line in preface.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_default_memory_template_line(trimmed) {
            continue;
        }
        if trimmed.starts_with("- ") && trimmed.ends_with(':') {
            continue;
        }
        lines.push(trimmed.to_string());
    }
    lines.join("\n")
}

fn is_default_memory_template_line(line: &str) -> bool {
    matches!(
        line,
        "# Long-Term Memory"
            | "This file is the agent's permanent memory. Keep it concise, current, and durable."
            | "## What Belongs Here"
            | "- Stable project architecture facts"
            | "- Repository conventions and workflows"
            | "- User preferences that affect future work"
            | "- Important decisions that should survive conversation resets"
            | "- Structured task summaries worth recalling later"
            | "## What Does Not Belong Here"
            | "- Full chat transcripts"
            | "- Temporary debugging notes"
            | "- Large logs or raw command output"
            | "## Suggested Sections"
            | "### Project"
            | "### Conventions"
            | "### User"
            | "## Memory Entries"
            | "Add durable entries below. Keep the newest relevant entries near the end."
    )
}

fn select_relevant_entries(topic: &str, entries: &[String], max_bytes: usize) -> Vec<String> {
    if entries.is_empty() || max_bytes == 0 {
        return Vec::new();
    }

    let topic_terms = topic_terms(topic);
    let mut scored = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let lowered = entry.to_ascii_lowercase();
            let mut score = topic_terms
                .iter()
                .filter(|term| lowered.contains(term.as_str()))
                .count();
            if lowered.contains("user instructed memory") {
                score += 1;
            }
            (idx, score, entry.clone())
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| right.0.cmp(&left.0)));

    let mut selected = Vec::new();
    let mut used = 0;
    for (_, score, entry) in scored {
        if !selected.is_empty() && score == 0 {
            break;
        }
        let entry_len = entry.len();
        if used + entry_len > max_bytes && !selected.is_empty() {
            break;
        }
        used += entry_len;
        selected.push(entry);
        if selected.len() >= 4 {
            break;
        }
    }

    if selected.is_empty() {
        for entry in entries.iter().rev().take(2).rev() {
            let entry_len = entry.len();
            if used + entry_len > max_bytes && !selected.is_empty() {
                break;
            }
            used += entry_len;
            selected.push(entry.clone());
        }
    }

    selected
}

fn topic_terms(topic: &str) -> Vec<String> {
    topic
        .split(|ch: char| !ch.is_alphanumeric())
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| part.len() > 2)
        .filter(|part| {
            !matches!(
                part.as_str(),
                "the"
                    | "and"
                    | "for"
                    | "with"
                    | "from"
                    | "that"
                    | "this"
                    | "into"
                    | "when"
                    | "need"
                    | "have"
                    | "about"
                    | "session"
                    | "task"
                    | "clear"
                    | "memorize"
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{MemoryEntry, MemoryEntryKind, MemoryStore};
    use tempfile::tempdir;

    #[test]
    fn memory_store_trims_to_latest_entries_with_limit() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path(), 2_000).unwrap();
        for idx in 0..8 {
            store
                .append_memory_entry(&MemoryEntry {
                    kind: MemoryEntryKind::TaskSummary,
                    title: format!("Entry {idx}"),
                    summary: format!("Summary {idx} {}", "x".repeat(80)),
                    attention_points: vec![format!("Point {idx}")],
                    recorded_at: "2026-03-24T00:00:00Z".to_string(),
                })
                .unwrap();
        }

        let content = store.read_long_term().unwrap();
        assert!(content.len() <= 2_000);
        assert!(content.contains("Entry 7"));
        assert!(!content.contains("Entry 0"));
    }

    #[test]
    fn memory_context_prefers_relevant_entries() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path(), 32 * 1024).unwrap();
        store
            .append_memory_entry(&MemoryEntry {
                kind: MemoryEntryKind::TaskSummary,
                title: "Slack thread handling".to_string(),
                summary: "Thread replies must preserve metadata.".to_string(),
                attention_points: vec!["Check thread_ts".to_string()],
                recorded_at: "2026-03-24T00:00:00Z".to_string(),
            })
            .unwrap();
        store
            .append_memory_entry(&MemoryEntry {
                kind: MemoryEntryKind::TaskSummary,
                title: "Cron cleanup".to_string(),
                summary: "Cron state should be refreshed safely.".to_string(),
                attention_points: vec!["Avoid stale timers".to_string()],
                recorded_at: "2026-03-24T00:00:00Z".to_string(),
            })
            .unwrap();

        let context = store
            .get_memory_context("fix slack thread stop behavior")
            .unwrap();
        assert!(context.contains("Slack thread handling"));
        assert!(!context.contains("Cron cleanup"));
    }

    #[test]
    fn reset_history_restores_template() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::new(dir.path(), 32 * 1024).unwrap();
        store.append_history("junk").unwrap();
        store.reset_history().unwrap();

        let history = std::fs::read_to_string(store.memory_dir().join("HISTORY.md")).unwrap();
        assert_eq!(history, crate::util::DEFAULT_HISTORY_TEMPLATE);
    }
}
