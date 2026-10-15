//! The Claude pane: ask Claude to work on the open presentation, as Copilot does in Office.
//!
//! The pane runs Claude Code (`claude`, installed and signed in on this computer) in print mode
//! with one MCP server, `pdfcraft-cli mcp --connect`, attached to this window through a control
//! channel of its own: a loopback port chosen by the system and a random token per run, so only
//! the Claude this pane starts can drive the app. Claude sees and changes the presentation the
//! person sees, with the same commands as the ribbon (each one undoable), and nothing else: the
//! MCP configuration is strict, Claude Code's own file, shell and web tools are refused, and it
//! runs in an empty folder of its own.
//!
//! Replies stream in as Claude Code's `stream-json` lines ([`parse_line`]); the conversation
//! continues across questions (`--resume`) until New chat.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::control::ControlRequest;

/// Starts a control channel for the pane: given the token requests must carry, the port it
/// listens on (127.0.0.1) and the requests it receives. The desktop app provides it.
pub type ControlStarter = Box<dyn Fn(&egui::Context, &str) -> Option<(u16, Receiver<ControlRequest>)>>;

/// What starts Claude for a question (the real one runs `claude`; tests use a fake).
pub type Launcher = Arc<dyn Fn(Launch) -> Result<Running, String> + Send + Sync>;

/// One question for Claude.
#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
    pub prompt: String,
    /// The conversation to continue.
    pub session: Option<String>,
    /// The MCP configuration file (the PdfCraft server, connected to this window).
    pub mcp_config: PathBuf,
    /// The folder Claude runs in.
    pub work_dir: PathBuf,
}

/// A question being answered: its events, and how to stop it.
pub struct Running {
    pub events: Receiver<Event>,
    pub stop: Box<dyn FnOnce() + Send>,
}

/// What Claude Code reports while it answers.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The conversation's id (to continue it).
    Session(String),
    /// What Claude says while it works ("Let me look at the slide…").
    Text(String),
    /// A tool it is using, in words ("Added a slide", "Ran slide.new").
    Step(String),
    /// The reply, once it has finished.
    Answer(String),
    /// Finished, with the error if it failed.
    Done(Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    /// The reply (Markdown).
    Claude,
    /// What Claude did on the way, one line each, folded under the reply.
    Work,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Msg {
    pub role: Role,
    pub text: String,
}

/// The longest conversation kept on screen.
const MAX_MESSAGES: usize = 400;
/// The longest line read from Claude Code (a slide image in a tool result can be large).
const MAX_LINE: u64 = 16 << 20;

/// Appended to Claude Code's system prompt.
pub const SYSTEM_PROMPT: &str = "You are the assistant inside PdfCraft, a PDF editor comparable to Adobe Acrobat, shown in a narrow side pane. \
The PDF the user has open is in the running app: work on it only through the pdfcraft tools, which act on exactly what the user sees \
(every change can be undone with Ctrl+Z). Read before you change: look at the pages and their text; make changes with the app's tools, and check the pages you changed before you finish. \
Reply in the language the user writes in. Work silently: don't narrate your steps. Your final reply is read in the narrow pane, so keep it short: \
one or two plain sentences, then at most five short bullet points saying what you changed or found. No headings, tables or code unless asked.";

/// Claude Code's own tools the pane refuses: Claude works on the PDF, not the computer.
pub const REFUSED_TOOLS: &[&str] =
    &["Bash", "BashOutput", "KillShell", "Edit", "MultiEdit", "Write", "NotebookEdit", "Read", "Glob", "Grep", "LS", "WebFetch", "WebSearch", "Task"];

pub struct Assistant {
    pub messages: Vec<Msg>,
    pub input: String,
    /// The conversation being continued.
    pub session: Option<String>,
    running: Option<Running>,
    /// What Claude last said on the way, shown as the reply if it finishes without one.
    last_note: Option<String>,
    /// The pane's control channel: requests from the Claude it started.
    pub control_rx: Option<Receiver<ControlRequest>>,
    port: Option<u16>,
    token: String,
    pub starter: Option<ControlStarter>,
    pub launcher: Launcher,
    /// `pdfcraft-cli` (default: beside the running executable).
    pub cli: Option<PathBuf>,
    work_dir: Option<PathBuf>,
    /// A question to send when the pane is next drawn (AI assistant ▸ Summarize).
    pub queued: Option<String>,
    /// Put the cursor in the input box when the pane is next drawn.
    pub focus: bool,
}

impl Default for Assistant {
    fn default() -> Self {
        Assistant {
            messages: Vec::new(),
            input: String::new(),
            session: None,
            running: None,
            last_note: None,
            control_rx: None,
            port: None,
            token: String::new(),
            starter: None,
            launcher: Arc::new(launch_claude),
            cli: None,
            work_dir: None,
            queued: None,
            focus: false,
        }
    }
}

impl Drop for Assistant {
    fn drop(&mut self) {
        self.stop();
        if let Some(d) = &self.work_dir {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

impl Assistant {
    pub fn busy(&self) -> bool {
        self.running.is_some()
    }

    fn push(&mut self, role: Role, text: impl Into<String>) {
        self.messages.push(Msg { role, text: text.into() });
        if self.messages.len() > MAX_MESSAGES {
            let extra = self.messages.len() - MAX_MESSAGES;
            self.messages.drain(..extra);
        }
    }

    /// Start a new conversation.
    pub fn new_chat(&mut self) {
        self.stop();
        self.messages.clear();
        self.session = None;
    }

    /// Stop the answer in progress.
    pub fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            (r.stop)();
            self.push(Role::Error, "Stopped.");
        }
    }

    /// The window's control channel (its port and token), opened by the app for the pane.
    pub fn set_channel(&mut self, port: u16, token: String) {
        self.port = Some(port);
        self.token = token;
    }

    pub fn has_channel(&self) -> bool {
        self.port.is_some()
    }

    /// The folder Claude runs in and the MCP configuration that connects it to this window.
    fn prepare(&mut self, _ctx: &egui::Context) -> Result<(PathBuf, PathBuf), String> {
        let port = self.port.ok_or("the Claude pane needs the PdfCraft desktop app")?;
        let dir = match &self.work_dir {
            Some(d) => d.clone(),
            None => {
                let d = std::env::temp_dir().join(format!("pdfcraft-claude-{}", std::process::id()));
                std::fs::create_dir_all(&d).map_err(|e| format!("couldn't create {}: {e}", d.display()))?;
                self.work_dir = Some(d.clone());
                d
            }
        };
        let cli = self.cli.clone().or_else(cli_path).ok_or("pdfcraft-cli wasn't found next to PdfCraft; reinstall PdfCraft")?;
        let config = mcp_config(&cli, port, &self.token);
        let path = dir.join("mcp.json");
        std::fs::write(&path, config.to_string()).map_err(|e| format!("couldn't write {}: {e}", path.display()))?;
        Ok((dir, path))
    }

    /// Ask Claude what's in the input box.
    pub fn send(&mut self, ctx: &egui::Context) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() || self.busy() {
            return;
        }
        self.input.clear();
        self.last_note = None;
        self.push(Role::User, prompt.clone());
        let launched = self
            .prepare(ctx)
            .and_then(|(work_dir, mcp_config)| (self.launcher)(Launch { prompt, session: self.session.clone(), mcp_config, work_dir }));
        match launched {
            Ok(r) => self.running = Some(r),
            Err(e) => self.push(Role::Error, e),
        }
    }

    /// Add a line to this answer's work (one folded entry under the question).
    fn work(&mut self, line: String) {
        match self.messages.last_mut() {
            Some(m) if m.role == Role::Work => {
                m.text.push('\n');
                m.text.push_str(&line);
            }
            _ => self.push(Role::Work, line),
        }
    }

    /// Take in what Claude has reported since the last frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let Some(run) = &self.running else { return };
        let mut events = Vec::new();
        let mut gone = false;
        loop {
            match run.events.try_recv() {
                Ok(e) => events.push(e),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    gone = true;
                    break;
                }
            }
        }
        let mut done = gone;
        for e in events {
            match e {
                Event::Session(id) => self.session = Some(id),
                Event::Text(t) => {
                    let first = t.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
                    let short: String = first.chars().take(140).collect();
                    self.work(format!("“{short}”"));
                    self.last_note = Some(t);
                }
                Event::Step(s) => self.work(s),
                Event::Answer(a) => {
                    self.last_note = None;
                    self.push(Role::Claude, a);
                }
                Event::Done(err) => {
                    match err {
                        Some(e) => self.push(Role::Error, e),
                        // Finished without a reply of its own: what it said last is the reply.
                        None => {
                            if let Some(n) = self.last_note.take() {
                                self.push(Role::Claude, n);
                            }
                        }
                    }
                    done = true;
                }
            }
        }
        if done {
            self.running = None;
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }
    }
}

/// `pdfcraft-cli` beside the running executable.
fn cli_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) { "pdfcraft-cli.exe" } else { "pdfcraft-cli" };
    let p = exe.parent()?.join(name);
    p.is_file().then_some(p)
}

/// The MCP configuration: the PdfCraft server, connected to this window with its token.
pub fn mcp_config(cli: &Path, port: u16, token: &str) -> Value {
    json!({
        "mcpServers": {
            "pdfcraft": {
                "type": "stdio",
                "command": cli.to_string_lossy(),
                "args": ["mcp", "--app", port.to_string()],
                "env": { "PDFCRAFT_CONTROL_TOKEN": token },
            }
        }
    })
}

/// Claude Code's command-line arguments for a question (the prompt goes in on stdin).
pub fn claude_args(l: &Launch) -> Vec<String> {
    let mut a: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose", "--strict-mcp-config", "--allowedTools", "mcp__pdfcraft"]
        .into_iter()
        .map(String::from)
        .collect();
    a.push("--mcp-config".into());
    a.push(l.mcp_config.to_string_lossy().into_owned());
    a.push("--disallowedTools".into());
    a.extend(REFUSED_TOOLS.iter().map(|t| t.to_string()));
    a.push("--append-system-prompt".into());
    a.push(SYSTEM_PROMPT.into());
    if let Some(s) = &l.session {
        a.push("--resume".into());
        a.push(s.clone());
    }
    a
}

/// Where Claude Code is: `DECKCRAFT_CLAUDE`, else `claude` on the PATH (on Windows the native
/// `claude.exe`, else npm's `claude.cmd`).
pub fn find_claude() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("DECKCRAFT_CLAUDE").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let names: &[&str] = if cfg!(windows) { &["claude.exe", "claude.cmd"] } else { &["claude"] };
    let path = std::env::var_os("PATH")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    names.iter().find_map(|n| dirs.iter().map(|d| d.join(n)).find(|p| p.is_file()))
}

/// The events in one `stream-json` line (none for lines the pane doesn't show).
pub fn parse_line(line: &str) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return Vec::new() };
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => s(&v, "session_id").map(Event::Session).into_iter().collect(),
        Some("assistant") => v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                        Some("text") => s(p, "text").filter(|t| !t.trim().is_empty()).map(|t| Event::Text(t.trim().to_string())),
                        Some("tool_use") => s(p, "name").map(|n| Event::Step(describe(&n, p.get("input").unwrap_or(&Value::Null)))),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Some("result") => {
            let mut out: Vec<Event> = s(&v, "session_id").map(Event::Session).into_iter().collect();
            let failed = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let text = s(&v, "result").map(|r| r.trim().to_string()).filter(|r| !r.is_empty());
            if failed {
                out.push(Event::Done(Some(text.or_else(|| s(&v, "subtype")).unwrap_or_else(|| "Claude couldn't finish".into()))));
            } else {
                out.extend(text.map(Event::Answer));
                out.push(Event::Done(None));
            }
            out
        }
        _ => Vec::new(),
    }
}

/// A tool call in words, for the folded list of what Claude did.
pub fn describe(tool: &str, input: &Value) -> String {
    let name = tool.trim_start_matches("mcp__pdfcraft__");
    let slide = input.get("slide").or_else(|| input.get("index")).and_then(Value::as_u64).map(|n| n + 1);
    let page = input.get("page").and_then(Value::as_u64);
    match name {
        "doc_list" => "Listed the open PDFs".into(),
        "doc_info" | "doc_inspect" => "Read the document's details".into(),
        "text_extract" => "Read the text".into(),
        "text_search" | "doc_find" => "Searched the text".into(),
        "page_render" | "render_preview" => page.map_or_else(|| "Looked at a page".into(), |n| format!("Looked at page {n}")),
        "comment_list" => "Read the comments".into(),
        "comment_add" => "Added a comment".into(),
        "text_add" | "edit_add_text" => "Added text".into(),
        "doc_save" => "Saved the PDF".into(),
        "ocr_recognize" => "Recognised the text".into(),
        "inspect_document" => "Read the presentation".into(),
        "inspect_slide" => slide.map_or_else(|| "Looked at a slide".into(), |n| format!("Looked at slide {n}")),
        "render_slide" => slide.map_or_else(|| "Checked how a slide looks".into(), |n| format!("Checked how slide {n} looks")),
        "list_commands" => "Looked up the commands".into(),
        "run_command" => input.get("command").and_then(Value::as_str).map_or_else(|| "Ran a command".into(), |c| format!("Ran {c}")),
        "batch" => match input.get("commands").and_then(Value::as_array).map(Vec::len) {
            Some(n) => format!("Ran {n} commands"),
            None => "Ran several commands".into(),
        },
        "add_slide" => "Added a slide".into(),
        "go_to_slide" => slide.map_or_else(|| "Went to a slide".into(), |n| format!("Went to slide {n}")),
        "add_shape" => "Added a shape".into(),
        "add_text_box" => "Added a text box".into(),
        "set_text" => "Changed text".into(),
        "insert_picture" => "Inserted a picture".into(),
        "insert_table" => "Inserted a table".into(),
        "insert_chart" => "Inserted a chart".into(),
        "new_presentation" => "Started a new presentation".into(),
        "open_presentation" => "Opened a presentation".into(),
        "save_presentation" => "Saved the presentation".into(),
        "export" => "Exported".into(),
        "screenshot" => "Looked at the window".into(),
        "select" => "Selected".into(),
        "select_tool" => "Picked a tool".into(),
        "pointer" => "Used the pointer".into(),
        "key" => "Pressed a key".into(),
        "type_text" => "Typed text".into(),
        "call_app" => "Asked the app".into(),
        other => {
            let words = other.replace('_', " ");
            let mut c = words.chars();
            c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
        }
    }
}

/// A run of inline Markdown: its text and how it is set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
}

/// Inline Markdown (`**bold**`, `*italic*` or `_italic_`, `` `code` ``) as spans; anything else,
/// unmatched markers included, is plain text.
pub fn spans(line: &str) -> Vec<Span> {
    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<Span> = Vec::new();
    let (mut bold, mut italic) = (false, false);
    let mut cur = String::new();
    let flush = |out: &mut Vec<Span>, cur: &mut String, bold: bool, italic: bool, code: bool| {
        if !cur.is_empty() {
            out.push(Span { text: std::mem::take(cur), bold, italic, code });
        }
    };
    let closes = |from: usize, pat: &[char]| -> bool { (from..chars.len()).any(|j| chars.get(j..j + pat.len()).is_some_and(|w| w == pat)) };
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if c == '`' && closes(i + 1, &['`']) {
            flush(&mut out, &mut cur, bold, italic, false);
            let end = (i + 1..chars.len()).find(|&j| chars.get(j) == Some(&'`')).unwrap_or(chars.len());
            let code: String = chars.get(i + 1..end).map(|c| c.iter().collect()).unwrap_or_default();
            out.push(Span { text: code, bold, italic, code: true });
            i = end + 1;
            continue;
        }
        if c == '*' && chars.get(i + 1) == Some(&'*') && (bold || closes(i + 2, &['*', '*'])) {
            flush(&mut out, &mut cur, bold, italic, false);
            bold = !bold;
            i += 2;
            continue;
        }
        let word_edge = |j: usize| j == 0 || chars.get(j - 1).is_none_or(|p| !p.is_alphanumeric());
        // An opening marker has text right after it and a closing one later; a closing marker
        // has text right before it.
        let opens = !italic && closes(i + 1, &[c]) && chars.get(i + 1).is_some_and(|n| !n.is_whitespace() && *n != c);
        let shuts = italic && i > 0 && chars.get(i - 1).is_some_and(|p| !p.is_whitespace());
        if (c == '*' || (c == '_' && (italic || word_edge(i)))) && (opens || shuts) {
            flush(&mut out, &mut cur, bold, italic, false);
            italic = !italic;
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    flush(&mut out, &mut cur, bold, italic, false);
    out
}

/// One line of the reply, as Markdown blocks go.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading(String),
    /// A list item: its marker ("•" or "3."), its nesting depth and its text.
    Item(String, usize, String),
    Quote(String),
    Code(String),
    Rule,
    Blank,
    Text(String),
}

/// The reply's Markdown, line by line (fenced code kept as it is).
pub fn blocks(text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut code: Option<Vec<String>> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            match code.take() {
                Some(lines) => out.push(Block::Code(lines.join("\n"))),
                None => code = Some(Vec::new()),
            }
            continue;
        }
        if let Some(lines) = code.as_mut() {
            lines.push(line.to_string());
            continue;
        }
        let trimmed = line.trim_start();
        let depth = (line.len() - trimmed.len()) / 2;
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && trimmed.chars().nth(hashes) == Some(' ') {
            out.push(Block::Heading(trimmed.get(hashes + 1..).unwrap_or_default().trim().to_string()));
        } else if trimmed.is_empty() {
            out.push(Block::Blank);
        } else if matches!(trimmed, "---" | "***" | "___") {
            out.push(Block::Rule);
        } else if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).or_else(|| trimmed.strip_prefix("+ ")) {
            out.push(Block::Item("•".into(), depth, rest.to_string()));
        } else if let Some(rest) = trimmed.strip_prefix("> ") {
            out.push(Block::Quote(rest.to_string()));
        } else {
            let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
            let after = trimmed.get(digits..).unwrap_or_default();
            match (digits, after.strip_prefix(". ").or_else(|| after.strip_prefix(") "))) {
                (1..=3, Some(rest)) => out.push(Block::Item(format!("{}.", trimmed.get(..digits).unwrap_or_default()), depth, rest.to_string())),
                _ => out.push(Block::Text(trimmed.to_string())),
            }
        }
    }
    if let Some(lines) = code {
        out.push(Block::Code(lines.join("\n")));
    }
    out
}

/// Draw inline Markdown wrapped to the pane.
fn rich(ui: &mut egui::Ui, text: &str, size: f32, color: egui::Color32, all_bold: bool) {
    let t = crate::theme::Tokens::get(ui.ctx());
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = ui.available_width();
    for sp in spans(text) {
        let font = if sp.code {
            egui::FontId::monospace(size - 1.0)
        } else if sp.bold || all_bold {
            crate::theme::semibold(size)
        } else {
            crate::theme::regular(size)
        };
        let format = egui::TextFormat {
            font_id: font,
            color,
            italics: sp.italic,
            background: if sp.code { t.hover } else { egui::Color32::TRANSPARENT },
            ..Default::default()
        };
        job.append(&sp.text, 0.0, format);
    }
    ui.add(egui::Label::new(job).wrap());
}

/// Draw a reply's Markdown: headings, lists, quotes, code, bold and italic.
pub fn markdown(ui: &mut egui::Ui, text: &str) {
    let t = crate::theme::Tokens::get(ui.ctx());
    let size = 13.5;
    for b in blocks(text) {
        match b {
            Block::Heading(h) => {
                ui.add_space(4.0);
                rich(ui, &h, size + 1.0, t.text, true);
            }
            Block::Item(marker, depth, rest) => {
                ui.horizontal_top(|ui| {
                    ui.add_space(4.0 + 14.0 * depth as f32);
                    ui.label(egui::RichText::new(marker).font(crate::theme::regular(size)).color(t.text_muted));
                    ui.vertical(|ui| rich(ui, &rest, size, t.text, false));
                });
            }
            Block::Quote(q) => {
                ui.horizontal_top(|ui| {
                    ui.add_space(6.0);
                    ui.vertical(|ui| rich(ui, &q, size, t.text_muted, false));
                });
            }
            Block::Code(c) => {
                egui::Frame::new().fill(t.hover).corner_radius(egui::CornerRadius::same(4)).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(egui::RichText::new(c).monospace().size(size - 1.0).color(t.text));
                });
            }
            Block::Rule => {
                ui.separator();
            }
            Block::Blank => ui.add_space(4.0),
            Block::Text(p) => rich(ui, &p, size, t.text, false),
        }
    }
}

/// Run Claude Code for a question.
fn launch_claude(l: Launch) -> Result<Running, String> {
    let claude = find_claude()
        .ok_or("Claude Code isn't installed: install it (https://claude.com/claude-code), run `claude` once to sign in, then ask again")?;
    let mut cmd = std::process::Command::new(&claude);
    cmd.args(claude_args(&l))
        .current_dir(&l.work_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // CREATE_NO_WINDOW: no console window flashes up.
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("couldn't start Claude Code ({}): {e}", claude.display()))?;
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = l.prompt.clone();
        std::thread::spawn(move || {
            let _ = stdin.write_all(prompt.as_bytes());
        });
    }
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let child = Arc::new(Mutex::new(child));
    let (tx, rx) = channel();
    let err_text = Arc::new(Mutex::new(String::new()));
    if let Some(mut e) = stderr {
        let err_text = err_text.clone();
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = (&mut e).take(64 << 10).read_to_string(&mut s);
            if let Ok(mut t) = err_text.lock() {
                *t = s;
            }
        });
    }
    let waiter = child.clone();
    std::thread::spawn(move || read_events(stdout, &tx, &waiter, &err_text));
    let killer = child;
    Ok(Running {
        events: rx,
        stop: Box::new(move || {
            if let Ok(mut c) = killer.lock() {
                let _ = c.kill();
            }
        }),
    })
}

fn read_events(stdout: Option<std::process::ChildStdout>, tx: &Sender<Event>, child: &Arc<Mutex<std::process::Child>>, err: &Arc<Mutex<String>>) {
    let mut finished = false;
    if let Some(out) = stdout {
        let mut reader = BufReader::new(out);
        loop {
            let mut buf = Vec::new();
            match reader.by_ref().take(MAX_LINE).read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    for e in parse_line(String::from_utf8_lossy(&buf).trim()) {
                        finished |= matches!(e, Event::Done(_));
                        if tx.send(e).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
    let status = child.lock().ok().and_then(|mut c| c.wait().ok());
    if !finished {
        // Ended without a result: say why, from its error output when it gave one.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let text = err.lock().map(|t| t.trim().lines().take(3).collect::<Vec<_>>().join(" ")).unwrap_or_default();
        let why = if !text.is_empty() {
            text
        } else if status.is_some_and(|s| s.success()) {
            "Claude ended without an answer".to_string()
        } else {
            "Claude Code stopped; run `claude` in a terminal once to check it is signed in".to_string()
        };
        let _ = tx.send(Event::Done(Some(why)));
    }
}

/// The pane's contents (inside the task pane frame).
/// What AI assistant ▸ Summarize asks.
pub const SUMMARIZE: &str = "Summarize the open PDF: what it is, who it is from and to, and its key points. Write the summary in the document's language.";

/// What AI assistant ▸ Translate starts the question with; the reader types the language.
pub const TRANSLATE: &str = "Translate the open PDF into ";

/// AI assistant ▸ Summarize, Ask about this document and Translate: Claude on the open PDF, in
/// the Claude pane. False for any other command.
pub fn tool(app: &mut crate::PdfCraftApp, command: &str) -> bool {
    let a = &mut app.assistant;
    match command {
        "ai.summary" => a.queued = Some(SUMMARIZE.into()),
        "ai.ask" => a.focus = true,
        "ai.translate" => {
            a.input = TRANSLATE.into();
            a.focus = true;
        }
        _ => return false,
    }
    app.assistant_open = true;
    true
}

pub fn pane(app: &mut crate::PdfCraftApp, ui: &mut egui::Ui) {
    let t = crate::theme::Tokens::get(ui.ctx());
    let a = &mut app.assistant;
    a.poll(ui.ctx());
    let input_h = 96.0;
    let list_h = (ui.available_height() - input_h).max(80.0);
    egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        if a.messages.is_empty() {
            ui.label(egui::RichText::new("Ask Claude to summarise, find, comment on or change the open PDF. Changes appear in the document as Claude makes them and can be undone.").color(t.text_muted));
        }
        for (i, m) in a.messages.iter().enumerate() {
            match m.role {
                Role::User => {
                    egui::Frame::new().fill(t.hover).corner_radius(egui::CornerRadius::same(8)).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(&m.text);
                    });
                }
                Role::Claude => markdown(ui, &m.text),
                Role::Work => {
                    // What Claude did, folded to one line ("5 steps"); open it to see them.
                    let n = m.text.lines().count();
                    let title = if n == 1 { "1 step".to_string() } else { format!("{n} steps") };
                    egui::CollapsingHeader::new(egui::RichText::new(title).small().color(t.text_muted)).id_salt(("assistant_work", i)).show(ui, |ui| {
                        for l in m.text.lines() {
                            ui.label(egui::RichText::new(format!("· {l}")).small().color(t.text_muted));
                        }
                    });
                }
                Role::Error => {
                    ui.label(egui::RichText::new(&m.text).color(egui::Color32::from_rgb(196, 43, 28)));
                }
            }
            ui.add_space(6.0);
        }
        if a.busy() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new("Claude is working…").color(t.text_muted));
            });
        }
    });
    ui.separator();
    let busy = a.busy();
    let edit = egui::TextEdit::multiline(&mut a.input).hint_text("Ask Claude…").desired_rows(2).desired_width(f32::INFINITY);
    let resp = ui.add_enabled(!busy, edit);
    if a.focus && !busy {
        a.focus = false;
        resp.request_focus();
        // The cursor after what is already there (Translate's "… into ").
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), resp.id) {
            let end = egui::text::CCursor::new(a.input.chars().count());
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
            state.store(ui.ctx(), resp.id);
        }
    }
    let enter = resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
    if enter {
        // Enter sends; Shift+Enter is a new line. Drop the newline the key typed.
        while a.input.ends_with('\n') {
            a.input.pop();
        }
    }
    let mut send = enter;
    if !busy && let Some(q) = a.queued.take() {
        a.input = q;
        send = true;
    }
    ui.horizontal(|ui| {
        if a.busy() {
            if ui.button("Stop").clicked() {
                a.stop();
            }
        } else if ui.add_enabled(!a.input.trim().is_empty(), egui::Button::new("Send")).clicked() {
            send = true;
        }
        if ui.add_enabled(!a.messages.is_empty(), egui::Button::new("New chat")).clicked() {
            a.new_chat();
        }
    });
    if send {
        if !app.assistant.has_channel() {
            app.open_assistant_channel(ui.ctx());
        }
        app.assistant.send(ui.ctx());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_json_lines_become_events() {
        assert_eq!(parse_line(r#"{"type":"system","subtype":"init","session_id":"abc","tools":[]}"#), vec![Event::Session("abc".into())]);
        let a = r#"{"type":"assistant","message":{"content":[{"type":"text","text":" Đã thêm slide. "},{"type":"tool_use","name":"mcp__pdfcraft__add_slide","input":{}}]}}"#;
        assert_eq!(parse_line(a), vec![Event::Text("Đã thêm slide.".into()), Event::Step("Added a slide".into())]);
        assert_eq!(
            parse_line(r#"{"type":"result","subtype":"success","is_error":false,"result":" Xong. ","session_id":"abc"}"#),
            vec![Event::Session("abc".into()), Event::Answer("Xong.".into()), Event::Done(None)]
        );
        assert_eq!(parse_line(r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#), vec![Event::Done(Some("error_max_turns".into()))]);
        for junk in ["", "not json", "{}", r#"{"type":"user"}"#, r#"{"type":"assistant","message":{}}"#, "[1,2]"] {
            assert!(parse_line(junk).is_empty(), "{junk}");
        }
    }

    #[test]
    fn claude_gets_only_the_pdfcraft_tools() {
        let l = Launch { prompt: "x".into(), session: Some("s1".into()), mcp_config: PathBuf::from("m.json"), work_dir: PathBuf::from(".") };
        let a = claude_args(&l);
        let has = |pair: [&str; 2]| a.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1]);
        assert!(a.contains(&"--strict-mcp-config".to_string()));
        assert!(has(["--allowedTools", "mcp__pdfcraft"]) && has(["--mcp-config", "m.json"]) && has(["--resume", "s1"]));
        for t in ["Bash", "Read", "Write", "Edit", "WebFetch"] {
            assert!(a.contains(&t.to_string()), "{t} refused");
        }
        assert!(!a.iter().any(|x| x == "x"), "the prompt goes in on stdin, not the command line");
        let cfg = mcp_config(Path::new("C:/PdfCraft/pdfcraft-cli.exe"), 4567, "tok");
        assert_eq!(cfg.pointer("/mcpServers/pdfcraft/args/2").and_then(Value::as_str), Some("4567"));
        assert_eq!(cfg.pointer("/mcpServers/pdfcraft/env/PDFCRAFT_CONTROL_TOKEN").and_then(Value::as_str), Some("tok"));
    }

    /// The pane end to end with a stand-in for Claude Code: the question goes out with the MCP
    /// configuration for this window's channel, and the reply, its steps and the conversation id
    /// come back into the pane.
    #[test]
    fn a_question_comes_back_as_a_reply_with_its_steps() {
        let mut a = Assistant::default();
        a.set_channel(4567, "tok".into());
        a.cli = Some(PathBuf::from("pdfcraft-cli"));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        a.launcher = Arc::new(move |l: Launch| {
            let config = std::fs::read_to_string(&l.mcp_config).map_err(|e| e.to_string())?;
            if let Ok(mut v) = log.lock() {
                v.push((l.session.clone(), config));
            }
            let (tx, rx) = channel();
            for e in [
                Event::Session("s1".into()),
                Event::Text("Để tôi đọc trang.".into()),
                Event::Step("Read the text".into()),
                Event::Answer("Tài liệu có 3 trang.".into()),
                Event::Done(None),
            ] {
                let _ = tx.send(e);
            }
            Ok(Running { events: rx, stop: Box::new(|| {}) })
        });
        let ctx = egui::Context::default();
        a.input = "Tóm tắt tài liệu".into();
        a.send(&ctx);
        a.poll(&ctx);
        let roles: Vec<Role> = a.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Work, Role::Claude]);
        a.input = "Thêm ghi chú".into();
        a.send(&ctx);
        a.poll(&ctx);
        let asked = asked.lock().map(|v| v.clone()).unwrap_or_default();
        assert_eq!((asked[0].0.as_deref(), asked[1].0.as_deref()), (None, Some("s1")));
        assert!(asked[0].1.contains(r#""--app","4567""#) && asked[0].1.contains("tok"), "{}", asked[0].1);
    }

    #[test]
    fn without_the_desktop_app_the_pane_says_so() {
        let mut a = Assistant::default();
        a.input = "hello".into();
        a.send(&egui::Context::default());
        assert_eq!(a.messages.last().map(|m| m.role), Some(Role::Error));
        assert!(!a.busy());
    }

    #[test]
    fn ai_assistant_tools_open_the_claude_pane() {
        let mut app = crate::PdfCraftApp::new();
        assert!(tool(&mut app, "ai.summary"));
        assert!(app.assistant_open);
        assert_eq!(app.assistant.queued.as_deref(), Some(SUMMARIZE));
        let mut app = crate::PdfCraftApp::new();
        assert!(tool(&mut app, "ai.translate"));
        assert!(app.assistant_open && app.assistant.focus);
        assert_eq!(app.assistant.input, TRANSLATE);
        let mut app = crate::PdfCraftApp::new();
        assert!(tool(&mut app, "ai.ask"));
        assert!(app.assistant_open && app.assistant.focus && app.assistant.queued.is_none());
        // Other commands are left to the registry.
        assert!(!tool(&mut app, "file.save"));
    }

    #[test]
    fn tool_calls_read_as_words() {
        assert_eq!(describe("mcp__pdfcraft__set_layout_name", &Value::Null), "Set layout name");
    }

    #[test]
    fn replies_are_read_as_markdown() {
        let b = |t: &str, bold: bool, italic: bool, code: bool| Span { text: t.into(), bold, italic, code };
        assert_eq!(
            spans("Đã **thêm** slide `slide.new` và *sửa* tiêu đề"),
            vec![
                b("Đã ", false, false, false),
                b("thêm", true, false, false),
                b(" slide ", false, false, false),
                b("slide.new", false, false, true),
                b(" và ", false, false, false),
                b("sửa", false, true, false),
                b(" tiêu đề", false, false, false),
            ]
        );
        // Unmatched markers and snake_case stay as they are.
        assert_eq!(spans("2 * 3 = 6, a_b_c, **x"), vec![b("2 * 3 = 6, a_b_c, **x", false, false, false)]);
        assert_eq!(
            blocks("## Kết quả\n\n- Slide 1\n  - Tiêu đề\n2. Hai\n> ghi chú\n---\n```\ncode\n```\nXong."),
            vec![
                Block::Heading("Kết quả".into()),
                Block::Blank,
                Block::Item("•".into(), 0, "Slide 1".into()),
                Block::Item("•".into(), 1, "Tiêu đề".into()),
                Block::Item("2.".into(), 0, "Hai".into()),
                Block::Quote("ghi chú".into()),
                Block::Rule,
                Block::Code("code".into()),
                Block::Text("Xong.".into()),
            ]
        );
    }
}
