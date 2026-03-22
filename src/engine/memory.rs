use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::storage::{ChatMessage, Session};
use crate::util::{ensure_dir, estimate_json_tokens, now_iso, workspace_state_dir};

pub struct MemoryStore {
    memory_dir: PathBuf,
    memory_file: PathBuf,
    history_file: PathBuf,
}

impl MemoryStore {
    pub fn new(workspace: &Path) -> Result<Self> {
        let memory_dir = ensure_dir(workspace_state_dir(workspace).join("memory"))?;
        Ok(Self {
            memory_file: memory_dir.join("MEMORY.md"),
            history_file: memory_dir.join("HISTORY.md"),
            memory_dir,
        })
    }

    pub fn read_long_term(&self) -> Result<String> {
        Ok(if self.memory_file.exists() {
            fs::read_to_string(&self.memory_file)?
        } else {
            String::new()
        })
    }

    pub fn write_long_term(&self, content: &str) -> Result<()> {
        fs::write(&self.memory_file, content)?;
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

    pub fn get_memory_context(&self) -> Result<String> {
        let long_term = self.read_long_term()?;
        Ok(if long_term.trim().is_empty() {
            String::new()
        } else {
            format!("## Long-term Memory\n{long_term}")
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
    pub fn new(workspace: &Path, context_window_tokens: usize) -> Result<Self> {
        Ok(Self {
            store: MemoryStore::new(workspace)?,
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
