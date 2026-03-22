use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;
use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, ExternalPrinter as RustylineExternalPrinter};

use rbot::providers::TextStreamCallback;
use rbot::util::{ensure_dir, workspace_state_dir};

const USER_EMOJI: &str = "🙂";
const BOT_EMOJI: &str = "🤖";

pub enum InputEvent {
    Prompt(String),
    Exit,
}

#[derive(Clone)]
struct Style {
    ansi: bool,
}

impl Style {
    fn detect() -> Self {
        Self {
            ansi: io::stdout().is_terminal(),
        }
    }

    fn paint(&self, code: &str, text: impl AsRef<str>) -> String {
        let text = text.as_ref();
        if self.ansi {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn accent(&self, text: impl AsRef<str>) -> String {
        self.paint("1;36", text)
    }

    fn dim(&self, text: impl AsRef<str>) -> String {
        self.paint("2", text)
    }

    fn error(&self, text: impl AsRef<str>) -> String {
        self.paint("1;31", text)
    }

    fn subtle(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;245", text)
    }

    fn keyword(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;75", text)
    }

    fn builtin(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;141", text)
    }

    fn string(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;180", text)
    }

    fn number(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;173", text)
    }

    fn comment(&self, text: impl AsRef<str>) -> String {
        self.paint("38;5;244", text)
    }

    fn code_fence(&self, lang: &str, open: bool) -> String {
        let marker = if open { "┌" } else { "└" };
        let label = if lang.is_empty() { "code" } else { lang };
        if self.ansi {
            format!(
                "{} {}",
                self.paint("48;5;236;38;5;255", format!(" {marker} {label} ")),
                self.dim(if open { "" } else { "" })
            )
            .trim_end()
            .to_string()
        } else {
            format!("{marker} {label}")
        }
    }

    fn tool_hint_pill(&self, parts: &ToolHintParts<'_>) -> String {
        let text = if parts.detail.is_empty() {
            format!("{} {}", parts.emoji, parts.tool_name)
        } else {
            format!("{} {}  {}", parts.emoji, parts.tool_name, parts.detail)
        };
        if self.ansi {
            self.paint("48;5;254;38;5;240", format!(" {text} "))
        } else {
            text
        }
    }

    fn queue_pill(&self, text: impl AsRef<str>) -> String {
        let text = text.as_ref();
        if self.ansi {
            self.paint("48;5;153;38;5;23", format!(" ⏳ {text} "))
        } else {
            format!("⏳ {text}")
        }
    }

    fn primary_prompt(&self) -> String {
        format!("{} ", self.accent(format!("{USER_EMOJI}›")))
    }

    fn continuation_prompt(&self) -> String {
        format!("{} ", self.dim("…›"))
    }
}

#[derive(Clone)]
struct OutputTarget {
    style: Style,
    printer: Option<SharedPrinter>,
}

impl OutputTarget {
    fn stdout(style: Style) -> Self {
        Self {
            style,
            printer: None,
        }
    }

    fn printer(style: Style, printer: SharedPrinter) -> Self {
        Self {
            style,
            printer: Some(printer),
        }
    }

    fn write_raw(&self, text: impl AsRef<str>) {
        let text = text.as_ref();
        if let Some(printer) = &self.printer {
            let _ = printer
                .lock()
                .expect("external printer lock poisoned")
                .print(text.to_string());
        } else {
            print!("{text}");
            let _ = io::stdout().flush();
        }
    }

    fn uses_external_printer(&self) -> bool {
        self.printer.is_some()
    }
}

fn write_stderr_raw(text: &str) {
    eprint!("{text}");
    let _ = io::stderr().flush();
}

type SharedPrinter = Arc<Mutex<Box<dyn RustylineExternalPrinter + Send>>>;

pub struct TurnSummary<'a> {
    pub model: &'a str,
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub elapsed: Duration,
}

pub struct CliShell {
    editor: DefaultEditor,
    history_path: PathBuf,
    style: Style,
    workspace: PathBuf,
    cwd: PathBuf,
    model: String,
    provider: String,
}

impl CliShell {
    pub fn new(
        workspace: &Path,
        cwd: &Path,
        model: impl Into<String>,
        provider: impl Into<String>,
    ) -> Result<Self> {
        let style = Style::detect();
        let history_path = history_file_path()?;
        let mut editor = DefaultEditor::new()?;
        let _ = editor.load_history(&history_path);
        Ok(Self {
            editor,
            history_path,
            style,
            workspace: workspace.to_path_buf(),
            cwd: cwd.to_path_buf(),
            model: model.into(),
            provider: provider.into(),
        })
    }

    pub fn print_welcome(&self) {
        let workspace = truncate_middle(&self.workspace.display().to_string(), 72);
        let cwd = truncate_middle(&self.cwd.display().to_string(), 72);
        let state_root = truncate_middle(
            &workspace_state_dir(&self.workspace).display().to_string(),
            72,
        );
        println!("{}", self.style.accent("╭─ rbot interactive"));
        println!(
            "{} {}",
            self.style.dim("│ model     "),
            self.style.accent(&self.model)
        );
        println!("{} {}", self.style.dim("│ provider  "), self.provider);
        println!("{} {}", self.style.dim("│ cwd       "), cwd);
        println!("{} {}", self.style.dim("│ workspace "), workspace);
        println!("{} {}", self.style.dim("│ state     "), state_root);
        println!(
            "{} {}",
            self.style.dim("│ input     "),
            self.style
                .subtle("queued turns allowed while a task is running")
        );
        println!(
            "{} {}",
            self.style.dim("╰ commands  "),
            self.style.dim("/help  /clear  /exit  /new  /status  /stop")
        );
    }

    pub fn create_output(&mut self) -> Result<CliOutput> {
        let printer = self
            .editor
            .create_external_printer()
            .ok()
            .map(|printer| Arc::new(Mutex::new(Box::new(printer) as Box<_>)));
        Ok(CliOutput {
            style: self.style.clone(),
            target: printer
                .map(|printer| OutputTarget::printer(self.style.clone(), printer))
                .unwrap_or_else(|| OutputTarget::stdout(self.style.clone())),
        })
    }

    pub fn read_event(&mut self) -> Result<InputEvent> {
        loop {
            self.print_turn_header();
            let line = match self.editor.readline(&self.style.primary_prompt()) {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => {
                    println!();
                    continue;
                }
                Err(ReadlineError::Eof) => return Ok(InputEvent::Exit),
                Err(err) => return Err(err.into()),
            };

            let input = self.read_multiline(line)?;
            let trimmed = input.trim();
            if trimmed.is_empty() {
                continue;
            }

            match parse_local_command(trimmed) {
                Some(LocalCommand::Exit) => return Ok(InputEvent::Exit),
                Some(LocalCommand::Help) => {
                    self.print_help();
                    continue;
                }
                Some(LocalCommand::Clear) => {
                    self.clear_screen();
                    continue;
                }
                None => {}
            }

            let _ = self.editor.add_history_entry(trimmed);
            let _ = self.editor.save_history(&self.history_path);
            return Ok(InputEvent::Prompt(trimmed.to_string()));
        }
    }

    pub fn stream_renderer(&self) -> StreamRenderer {
        StreamRenderer::new(OutputTarget::stdout(self.style.clone()))
    }

    fn print_help(&self) {
        println!("{}", self.style.accent("╭─ CLI Help"));
        println!(
            "{} {}",
            self.style.dim("│ local    "),
            "/help  /clear  /exit"
        );
        println!(
            "{} {}",
            self.style.dim("│ agent    "),
            "/new  /status  /stop"
        );
        println!(
            "{} {}",
            self.style.dim("│ input    "),
            "end a line with \\ for multiline input"
        );
        println!(
            "{} {}",
            self.style.dim("│ queue    "),
            "new prompts entered during a running turn are queued automatically"
        );
        println!(
            "{} {}",
            self.style.dim("╰ history  "),
            "~/.rbot/history.txt"
        );
    }

    fn clear_screen(&self) {
        if self.style.ansi {
            print!("\x1b[2J\x1b[H");
            let _ = io::stdout().flush();
        } else {
            println!("\n\n");
        }
    }

    fn print_turn_header(&self) {
        let cwd_name = self
            .cwd
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| self.cwd.display().to_string());
        println!(
            "{} {} {} {} {}",
            self.style.dim("·"),
            self.style.accent(&self.model),
            self.style.dim("·"),
            self.provider,
            self.style.dim(format!("· {cwd_name}"))
        );
    }

    fn read_multiline(&mut self, mut current: String) -> Result<String> {
        while line_requests_continuation(&current) {
            current.pop();
            let next = match self.editor.readline(&self.style.continuation_prompt()) {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => {
                    println!();
                    continue;
                }
                Err(ReadlineError::Eof) => break,
                Err(err) => return Err(err.into()),
            };
            current.push('\n');
            current.push_str(&next);
        }
        Ok(current)
    }
}

#[derive(Clone)]
pub struct CliOutput {
    style: Style,
    target: OutputTarget,
}

impl CliOutput {
    pub fn stream_renderer(&self) -> StreamRenderer {
        StreamRenderer::new(self.target.clone())
    }

    pub fn print_queue_notice(&self, queued: usize, prompt: &str) {
        let preview = truncate_middle(prompt.trim(), 64);
        self.target.write_raw(format!(
            "\n{}\n",
            self.style
                .queue_pill(format!("queued #{queued} · {}", preview))
        ));
    }

    pub fn print_dequeue_notice(&self, remaining: usize, prompt: &str) {
        let preview = truncate_middle(prompt.trim(), 64);
        self.target.write_raw(format!(
            "\n{}\n",
            self.style.queue_pill(format!(
                "running queued turn · {}{}",
                preview,
                if remaining > 0 {
                    format!(" · {remaining} still queued")
                } else {
                    String::new()
                }
            ))
        ));
    }

    pub fn print_exit_notice(&self) {
        self.target.write_raw(format!(
            "\n{}\n",
            self.style
                .queue_pill("exit requested · finishing the current turn first")
        ));
    }
}

#[derive(Clone)]
pub struct StreamRenderer {
    target: OutputTarget,
    state: Arc<Mutex<StreamState>>,
}

#[derive(Default)]
struct StreamState {
    started: bool,
    waiting: bool,
    pending: String,
    code_language: Option<String>,
    trailing_newlines: usize,
    spinner_stop: Option<Arc<AtomicBool>>,
    spinner_handle: Option<JoinHandle<()>>,
}

impl StreamRenderer {
    fn new(target: OutputTarget) -> Self {
        Self {
            target,
            state: Arc::new(Mutex::new(StreamState::default())),
        }
    }

    pub fn start_waiting(&self) {
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if state.started || state.waiting {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let target = self.target.clone();
        let use_stderr = target.uses_external_printer();
        let handle = thread::spawn(move || {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut idx = 0usize;
            while !stop_flag.load(Ordering::SeqCst) {
                let frame = target
                    .style
                    .dim(format!("{BOT_EMOJI} {}", frames[idx % frames.len()]));
                if use_stderr {
                    write_stderr_raw(&format!("\r\x1b[2K{frame}"));
                } else {
                    target.write_raw(format!("\r\x1b[2K{frame}"));
                }
                idx += 1;
                thread::sleep(Duration::from_millis(90));
            }
        });
        state.spinner_stop = Some(stop);
        state.spinner_handle = Some(handle);
        state.waiting = true;
    }

    pub fn callback(&self) -> TextStreamCallback {
        let target = self.target.clone();
        let state = self.state.clone();
        Arc::new(Mutex::new(Box::new(move |delta: String| {
            let mut state = state.lock().expect("cli stream state lock poisoned");
            if !state.started {
                if state.waiting {
                    stop_waiting_indicator(&target, &mut state);
                    state.waiting = false;
                }
                target.write_raw(format!("\n{} ", target.style.accent(BOT_EMOJI)));
                state.started = true;
                note_output(&mut state, "\n");
            }
            let rendered = render_stream_delta(
                &target.style,
                &mut state,
                &delta,
                false,
                target.uses_external_printer(),
            );
            if !rendered.is_empty() {
                target.write_raw(&rendered);
                note_output(&mut state, &rendered);
            }
        })))
    }

    pub fn tool_hint(&self, hint: &str) {
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if state.waiting {
            stop_waiting_indicator(&self.target, &mut state);
            state.waiting = false;
            if !state.started {
                self.target
                    .write_raw(format!("\n{} ", self.target.style.accent(BOT_EMOJI)));
                state.started = true;
                note_output(&mut state, "\n");
            }
        }
        let parts = parse_tool_hint(hint);
        let pill = self.target.style.tool_hint_pill(&parts);
        let prefix = if state.trailing_newlines == 0 {
            "\n"
        } else {
            ""
        };
        let rendered = format!("{prefix}{pill}\n");
        self.target.write_raw(&rendered);
        note_output(&mut state, &rendered);
    }

    pub fn finish(&self, content: &str, summary: &TurnSummary<'_>) {
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if state.waiting {
            stop_waiting_indicator(&self.target, &mut state);
            state.waiting = false;
        }
        if state.started {
            let tail = render_stream_delta(
                &self.target.style,
                &mut state,
                "",
                true,
                self.target.uses_external_printer(),
            );
            if !tail.is_empty() {
                self.target.write_raw(&tail);
                note_output(&mut state, &tail);
            }
            self.target.write_raw("\n\n");
            note_output(&mut state, "\n\n");
        } else {
            let rendered = render_markdown_response(&self.target.style, content);
            let rendered = format!("\n{} {}\n\n", self.target.style.accent(BOT_EMOJI), rendered);
            self.target.write_raw(&rendered);
            note_output(&mut state, &rendered);
        }
        let footer = format!(
            "{}\n",
            self.target.style.dim(format!(
                "╰ {} · {} in · {} out · {:.1}s",
                summary.model,
                summary.prompt_tokens,
                summary.completion_tokens,
                summary.elapsed.as_secs_f64()
            ))
        );
        self.target.write_raw(&footer);
        note_output(&mut state, &footer);
    }

    pub fn finish_empty(&self, note: &str, summary: &TurnSummary<'_>) {
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if state.waiting {
            stop_waiting_indicator(&self.target, &mut state);
            state.waiting = false;
        }
        let tail = render_stream_delta(
            &self.target.style,
            &mut state,
            "",
            true,
            self.target.uses_external_printer(),
        );
        if !tail.is_empty() {
            self.target.write_raw(&tail);
            note_output(&mut state, &tail);
        }
        if !note.trim().is_empty() {
            let rendered = format!("\n{}\n", self.target.style.subtle(format!("· {note}")));
            self.target.write_raw(&rendered);
            note_output(&mut state, &rendered);
        }
        let footer = format!(
            "{}\n",
            self.target.style.dim(format!(
                "╰ {} · {} in · {} out · {:.1}s",
                summary.model,
                summary.prompt_tokens,
                summary.completion_tokens,
                summary.elapsed.as_secs_f64()
            ))
        );
        self.target.write_raw(&footer);
        note_output(&mut state, &footer);
    }

    pub fn finish_error(&self, err: &str) {
        let mut state = self.state.lock().expect("cli stream state lock poisoned");
        if state.waiting {
            stop_waiting_indicator(&self.target, &mut state);
            state.waiting = false;
        }
        let tail = render_stream_delta(
            &self.target.style,
            &mut state,
            "",
            true,
            self.target.uses_external_printer(),
        );
        if !tail.is_empty() {
            self.target.write_raw(&tail);
            note_output(&mut state, &tail);
        }
        let rendered = format!("\n{} {}\n", self.target.style.error("error:"), err);
        self.target.write_raw(&rendered);
        note_output(&mut state, &rendered);
    }
}

fn stop_waiting_indicator(target: &OutputTarget, state: &mut StreamState) {
    if let Some(stop) = state.spinner_stop.take() {
        stop.store(true, Ordering::SeqCst);
    }
    if let Some(handle) = state.spinner_handle.take() {
        let _ = handle.join();
    }
    if target.uses_external_printer() {
        write_stderr_raw("\r\x1b[2K");
    } else {
        target.write_raw("\r\x1b[2K");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalCommand {
    Help,
    Clear,
    Exit,
}

fn parse_local_command(input: &str) -> Option<LocalCommand> {
    match input.trim() {
        "/help" => Some(LocalCommand::Help),
        "/clear" => Some(LocalCommand::Clear),
        "/exit" | "/quit" | "exit" | "quit" => Some(LocalCommand::Exit),
        _ => None,
    }
}

fn line_requests_continuation(input: &str) -> bool {
    input.ends_with('\\') && !input.ends_with("\\\\")
}

fn history_file_path() -> Result<PathBuf> {
    let root = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".rbot");
    ensure_dir(&root)?;
    Ok(root.join("history.txt"))
}

fn parse_tool_hint(hint: &str) -> ToolHintParts<'_> {
    let (tool_name, detail) = hint.split_once(" · ").unwrap_or((hint, ""));
    ToolHintParts {
        emoji: tool_emoji(tool_name),
        tool_name,
        detail,
    }
}

struct ToolHintParts<'a> {
    emoji: &'static str,
    tool_name: &'a str,
    detail: &'a str,
}

fn tool_emoji(tool_name: &str) -> &'static str {
    match tool_name {
        "read_file" => "📄",
        "write_file" | "edit_file" => "✍️",
        "list_dir" => "🗂️",
        "exec" => "🖥️",
        "web_search" => "🔎",
        "web_fetch" => "🌐",
        "message" => "💬",
        "spawn" => "🧵",
        "cron" => "⏱️",
        name if name.starts_with("mcp_") => "🧩",
        _ => "⚙️",
    }
}

fn render_markdown_response(style: &Style, content: &str) -> String {
    let mut state = StreamState::default();
    render_stream_delta(style, &mut state, content, true, false)
}

fn render_stream_delta(
    style: &Style,
    state: &mut StreamState,
    delta: &str,
    flush_all: bool,
    line_safe_mode: bool,
) -> String {
    state.pending.push_str(delta);
    let mut out = String::new();
    loop {
        if let Some(pos) = state.pending.find('\n') {
            let line = state.pending[..=pos].to_string();
            state.pending.replace_range(..=pos, "");
            out.push_str(&render_stream_line(style, state, &line));
            continue;
        }
        if flush_all {
            if !state.pending.is_empty() {
                let line = std::mem::take(&mut state.pending);
                out.push_str(&render_stream_line(style, state, &line));
            }
            break;
        }
        if state.code_language.is_none() {
            if line_safe_mode {
                if let Some(index) = sentence_flush_index(&state.pending) {
                    let chunk = state.pending[..index].to_string();
                    state.pending.replace_range(..index, "");
                    out.push_str(&chunk);
                    continue;
                }
            } else if !state.pending.is_empty() {
                out.push_str(&state.pending);
                state.pending.clear();
            }
        }
        break;
    }
    out
}

fn sentence_flush_index(text: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (idx, ch) in text.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            quote = Some(ch);
            continue;
        }
        if matches!(ch, '.' | '!' | '?' | ':' | ';') {
            let next = text[idx + ch.len_utf8()..].chars().next();
            if next.is_none_or(char::is_whitespace) {
                return Some(idx + ch.len_utf8());
            }
        }
    }
    None
}

fn note_output(state: &mut StreamState, text: &str) {
    if text.is_empty() {
        return;
    }
    let trailing = text.chars().rev().take_while(|ch| *ch == '\n').count();
    if trailing > 0 {
        state.trailing_newlines = trailing;
    } else {
        state.trailing_newlines = 0;
    }
}

fn render_stream_line(style: &Style, state: &mut StreamState, line: &str) -> String {
    let has_newline = line.ends_with('\n');
    let normalized = line.trim_end_matches('\n').trim_end_matches('\r');
    if let Some(lang) = fence_language(normalized) {
        if state.code_language.is_none() {
            state.code_language = Some(lang.to_string());
            return format!("\n{}\n", style.code_fence(lang, true));
        }
        let previous = state.code_language.take().unwrap_or_default();
        return format!("{}\n", style.code_fence(&previous, false));
    }
    if let Some(lang) = state.code_language.as_deref() {
        let highlighted = highlight_code_line(style, normalized, lang);
        if has_newline {
            format!("{highlighted}\n")
        } else {
            highlighted
        }
    } else {
        line.to_string()
    }
}

fn fence_language(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    trimmed.strip_prefix("```").map(|lang| lang.trim())
}

fn highlight_code_line(style: &Style, line: &str, language: &str) -> String {
    if !style.ansi || line.is_empty() {
        return line.to_string();
    }
    let language = normalize_language(language);
    let (code, comment) = split_comment(line, language);
    let mut out = highlight_code_tokens(style, code, language);
    if let Some(comment) = comment {
        out.push_str(&style.comment(comment));
    }
    out
}

fn normalize_language(language: &str) -> &str {
    match language.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "sh" => "bash",
        "zsh" => "bash",
        "yml" => "yaml",
        "md" => "markdown",
        "jsonc" => "json",
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}

fn split_comment<'a>(line: &'a str, language: &str) -> (&'a str, Option<&'a str>) {
    let delimiter = match language {
        "python" | "bash" | "yaml" | "dockerfile" | "ruby" => Some("#"),
        "sql" => Some("--"),
        "rust" | "javascript" | "typescript" | "jsx" | "tsx" | "java" | "go" | "c" | "cpp"
        | "swift" | "kotlin" | "csharp" => Some("//"),
        _ => None,
    };
    let Some(delimiter) = delimiter else {
        return (line, None);
    };
    let Some(index) = find_comment_start(line, delimiter) else {
        return (line, None);
    };
    (&line[..index], Some(&line[index..]))
}

fn find_comment_start(line: &str, delimiter: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            quote = Some(ch);
            continue;
        }
        if line[idx..].starts_with(delimiter) {
            return Some(idx);
        }
    }
    None
}

fn highlight_code_tokens(style: &Style, code: &str, language: &str) -> String {
    let keywords = language_keywords(language);
    let builtins = builtin_keywords(language);
    let mut out = String::new();
    let chars = code.chars().collect::<Vec<_>>();
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '"' || ch == '\'' || ch == '`' {
            let start = index;
            index += 1;
            let mut escaped = false;
            while index < chars.len() {
                let current = chars[index];
                if escaped {
                    escaped = false;
                } else if current == '\\' {
                    escaped = true;
                } else if current == ch {
                    index += 1;
                    break;
                }
                index += 1;
            }
            out.push_str(&style.string(chars[start..index].iter().collect::<String>()));
            continue;
        }
        if is_number_start(&chars, index) {
            let start = index;
            index += 1;
            while index < chars.len()
                && (chars[index].is_ascii_hexdigit()
                    || matches!(chars[index], '_' | '.' | 'x' | 'X' | 'o' | 'O' | 'b' | 'B'))
            {
                index += 1;
            }
            out.push_str(&style.number(chars[start..index].iter().collect::<String>()));
            continue;
        }
        if is_identifier_start(ch) {
            let start = index;
            index += 1;
            while index < chars.len() && is_identifier_continue(chars[index]) {
                index += 1;
            }
            let token = chars[start..index].iter().collect::<String>();
            if keywords.contains(&token.as_str()) {
                out.push_str(&style.keyword(token));
            } else if builtins.contains(&token.as_str()) {
                out.push_str(&style.builtin(token));
            } else {
                out.push_str(&token);
            }
            continue;
        }
        out.push(ch);
        index += 1;
    }
    out
}

fn language_keywords(language: &str) -> &'static [&'static str] {
    match language {
        "rust" => &[
            "fn", "let", "mut", "pub", "impl", "struct", "enum", "trait", "async", "await",
            "match", "if", "else", "for", "while", "loop", "return", "use", "mod", "const",
            "static", "where", "Self", "self",
        ],
        "python" => &[
            "def", "class", "async", "await", "if", "elif", "else", "for", "while", "return",
            "import", "from", "try", "except", "finally", "with", "as", "pass", "yield",
        ],
        "javascript" | "typescript" | "jsx" | "tsx" => &[
            "function", "const", "let", "var", "class", "extends", "async", "await", "if", "else",
            "for", "while", "return", "import", "from", "export", "new", "switch", "case",
            "default", "try", "catch",
        ],
        "bash" => &[
            "if", "then", "else", "fi", "for", "do", "done", "case", "esac", "function", "in",
        ],
        "json" => &[],
        _ => &[
            "if", "else", "for", "while", "return", "class", "function", "const", "let", "var",
        ],
    }
}

fn builtin_keywords(language: &str) -> &'static [&'static str] {
    match language {
        "rust" => &["Some", "None", "Ok", "Err", "true", "false"],
        "python" => &["True", "False", "None"],
        "javascript" | "typescript" | "jsx" | "tsx" => &["true", "false", "null", "undefined"],
        "json" => &["true", "false", "null"],
        _ => &["true", "false", "null"],
    }
}

fn is_identifier_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_identifier_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn is_number_start(chars: &[char], index: usize) -> bool {
    let ch = chars[index];
    if !ch.is_ascii_digit() {
        return false;
    }
    if index > 0 && is_identifier_continue(chars[index - 1]) {
        return false;
    }
    true
}

fn truncate_middle(text: &str, max_chars: usize) -> String {
    let chars = text.chars().count();
    if chars <= max_chars {
        return text.to_string();
    }
    let head = max_chars / 2 - 2;
    let tail = max_chars.saturating_sub(head + 3);
    let start = text.chars().take(head).collect::<String>();
    let end = text
        .chars()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{start}...{end}")
}

#[cfg(test)]
mod tests {
    use super::{
        LocalCommand, StreamState, fence_language, highlight_code_line, line_requests_continuation,
        note_output, parse_local_command, parse_tool_hint, sentence_flush_index, truncate_middle,
    };

    #[test]
    fn parses_local_commands() {
        assert_eq!(parse_local_command("/help"), Some(LocalCommand::Help));
        assert_eq!(parse_local_command("/clear"), Some(LocalCommand::Clear));
        assert_eq!(parse_local_command("quit"), Some(LocalCommand::Exit));
        assert_eq!(parse_local_command("/status"), None);
    }

    #[test]
    fn detects_multiline_continuation() {
        assert!(line_requests_continuation("hello \\"));
        assert!(!line_requests_continuation("hello"));
        assert!(!line_requests_continuation("hello \\\\"));
    }

    #[test]
    fn truncates_middle_for_long_paths() {
        let value = truncate_middle("/very/long/path/to/a/project/workspace/directory", 20);
        assert!(value.contains("..."));
        assert!(value.len() <= 20);
    }

    #[test]
    fn parses_tool_hint_and_emoji() {
        let parts = parse_tool_hint("read_file · path=src/main.rs");
        assert_eq!(parts.emoji, "📄");
        assert_eq!(parts.tool_name, "read_file");
        assert_eq!(parts.detail, "path=src/main.rs");
    }

    #[test]
    fn detects_fence_language() {
        assert_eq!(fence_language("```rust"), Some("rust"));
        assert_eq!(fence_language("```"), Some(""));
        assert_eq!(fence_language("fn main() {}"), None);
    }

    #[test]
    fn highlights_rust_keywords_when_ansi_enabled() {
        let style = super::Style { ansi: true };
        let rendered = highlight_code_line(&style, "let value = Some(42);", "rust");
        assert!(rendered.contains("\u{1b}[38;5;75mlet\u{1b}[0m"));
        assert!(rendered.contains("\u{1b}[38;5;141mSome\u{1b}[0m"));
    }

    #[test]
    fn sentence_flush_finds_boundary() {
        assert_eq!(sentence_flush_index("Hello world. Next"), Some(12));
        assert_eq!(sentence_flush_index("path=src/main.rs"), None);
    }

    #[test]
    fn note_output_tracks_trailing_newlines() {
        let mut state = StreamState::default();
        note_output(&mut state, "hello\n\n");
        assert_eq!(state.trailing_newlines, 2);
        note_output(&mut state, "world");
        assert_eq!(state.trailing_newlines, 0);
    }
}
