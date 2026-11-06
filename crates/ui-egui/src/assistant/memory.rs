//! The assistant's memory of each file: the conversation about it, kept on this computer so it is
//! still there when the file is opened again, and work on it continues where it stopped.
//!
//! All the Craft apps keep it in one folder, `ArtCraft/AI` in the system's settings folder
//! (`%APPDATA%\ArtCraft\AI` on Windows; [`Store::shared`]): `memory/<hash of the path>.json`
//! per document, and `work`, the folder the assistant runs in. Each record holds the messages on screen, who answered and that assistant's conversation id. Nothing is
//! written into the document itself unless the person asks for it (see [`Memory::to_json`],
//! which the file writers embed). Claude Code and Codex keep their own conversation on this
//! computer: the id continues it here; elsewhere (or once it is gone) the next question carries a
//! recap of the earlier messages instead ([`recap`]).
//!
//! The same file lives in every Craft app (`E:\Github\_shared\assistant_memory.rs`).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{Msg, Role};

/// The memory format's version, written into every record.
const VERSION: u64 = 1;
/// The largest record kept (the oldest messages go first).
const MAX_BYTES: usize = 2 << 20;
/// How much of the earlier conversation a recap carries.
const RECAP_CHARS: usize = 12_000;

/// What is remembered about one file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Memory {
    pub messages: Vec<Msg>,
    /// Who answered (`Provider::key`).
    pub provider: Option<String>,
    /// That assistant's conversation id, to continue it on this computer.
    pub session: Option<String>,
}

fn role_key(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Claude => "assistant",
        Role::Work => "work",
        Role::Error => "error",
    }
}

fn role_from(key: &str) -> Option<Role> {
    match key {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Claude),
        "work" => Some(Role::Work),
        "error" => Some(Role::Error),
        _ => None,
    }
}

impl Memory {
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// The record as JSON (what is saved, and what a file embeds). The conversation id is left out
    /// with `portable`: it only means something on this computer.
    pub fn to_json(&self, portable: bool) -> Value {
        let mut messages: Vec<Value> = self.messages.iter().map(|m| json!({"role": role_key(m.role), "text": m.text})).collect();
        // Keep it bounded: drop the oldest messages until it fits.
        let mut size: usize = messages.iter().map(|m| m.to_string().len()).sum();
        while size > MAX_BYTES && !messages.is_empty() {
            size -= messages.remove(0).to_string().len();
        }
        let mut v = json!({"version": VERSION, "messages": messages});
        if let Some(p) = &self.provider {
            v["provider"] = json!(p);
        }
        if !portable && let Some(s) = &self.session {
            v["session"] = json!(s);
        }
        v
    }

    /// Reads a record back; anything unreadable is skipped, a newer version is read as far as it
    /// is understood.
    pub fn from_json(v: &Value) -> Memory {
        let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let messages = v
            .get("messages")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|m| Some(Msg { role: role_from(m.get("role")?.as_str()?)?, text: text(m, "text")? })).collect())
            .unwrap_or_default();
        Memory { messages, provider: text(v, "provider"), session: text(v, "session").filter(|s| !s.is_empty()) }
    }
}

/// The earlier conversation, for an assistant that can't continue it: what was asked and
/// answered (not the work steps or errors), the latest part when it is long.
pub fn recap(messages: &[Msg]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut size = 0;
    for m in messages.iter().rev() {
        let who = match m.role {
            Role::User => "User",
            Role::Claude => "Assistant",
            Role::Work | Role::Error => continue,
        };
        let line = format!("{who}: {}", m.text.trim());
        size += line.chars().count();
        if size > RECAP_CHARS && !lines.is_empty() {
            break;
        }
        lines.push(line);
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n\n"))
}

/// The question with the earlier conversation in front of it.
pub fn with_recap(recap: &str, prompt: &str) -> String {
    format!(
        "Earlier conversation about this file (the previous session can't be continued; use it as context, and check the file itself before relying on it):\n\n{recap}\n\n---\n\n{prompt}"
    )
}

/// Where the memories live (see the module's documentation).
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

/// FNV-1a: a hash that is the same on every run and every computer (std's is not).
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// The same file however its path is written (on Windows: case and slashes).
fn normal(path: &str) -> String {
    let p = std::fs::canonicalize(path).map_or_else(|_| path.to_string(), |p| p.to_string_lossy().into_owned());
    let p = p.strip_prefix(r"\\?\").unwrap_or(&p).to_string();
    if cfg!(windows) { p.replace('/', "\\").to_lowercase() } else { p }
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Store {
        Store { dir: dir.into() }
    }

    /// The folder every Craft app shares: `ArtCraft/AI` in the system's settings folder. None
    /// with `ARTCRAFT_NO_AI_MEMORY` set (test runs).
    pub fn shared() -> Option<Store> {
        if std::env::var_os("ARTCRAFT_NO_AI_MEMORY").is_some() {
            return None;
        }
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let base = if cfg!(target_os = "macos") {
            var("HOME").map(|h| h.join("Library/Application Support"))
        } else if cfg!(windows) {
            var("APPDATA")
        } else {
            var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|h| h.join(".config")))
        }?;
        Some(Store::new(base.join("ArtCraft").join("AI")))
    }

    /// The folder the assistant runs in: the same one every time, so its conversations can be
    /// continued after the app restarts.
    pub fn work_dir(&self) -> PathBuf {
        self.dir.join("work")
    }

    fn file(&self, path: &str) -> PathBuf {
        self.dir.join("memory").join(format!("{:016x}.json", fnv(&normal(path))))
    }

    /// The memory of the document at `path` (empty when there is none).
    pub fn load(&self, path: &str) -> Memory {
        let Ok(b) = std::fs::read(self.file(path)) else { return Memory::default() };
        let Ok(v) = serde_json::from_slice::<Value>(&b) else { return Memory::default() };
        // A hash shared by two paths (vanishingly rare) is not the other file's memory.
        if v.get("path").and_then(Value::as_str).is_some_and(|p| p != normal(path)) {
            return Memory::default();
        }
        Memory::from_json(&v)
    }

    /// Remembers `m` for `path`; an empty memory forgets it.
    pub fn save(&self, path: &str, m: &Memory) {
        let f = self.file(path);
        if m.is_empty() {
            let _ = std::fs::remove_file(&f);
            return;
        }
        let mut v = m.to_json(false);
        v["path"] = json!(normal(path));
        if f.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) {
            // Written beside and renamed, so a crash never leaves half a record.
            let tmp = f.with_extension("json.tmp");
            if std::fs::write(&tmp, v.to_string()).is_ok() {
                let _ = std::fs::rename(&tmp, &f);
            }
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl super::Assistant {
    /// Remember conversations in `store` from now on; the document on screen brings its own back
    /// when nothing has been asked yet.
    pub fn use_memory(&mut self, store: Option<Store>) {
        self.memory = store;
        if self.messages.is_empty() && !self.busy() {
            self.doc = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, text: &str) -> Msg {
        Msg { role, text: text.into() }
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("craft-assistant-memory-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_memory_round_trips_and_forgets() {
        let d = scratch("roundtrip");
        let store = Store::new(&d);
        let m = Memory {
            messages: vec![msg(Role::User, "Tô màu cột B"), msg(Role::Work, "Ran home.fillColor"), msg(Role::Claude, "Đã tô màu.")],
            provider: Some("claude".into()),
            session: Some("abc".into()),
        };
        store.save("C:/Docs/Báo cáo.xlsx", &m);
        assert_eq!(store.load("C:/Docs/Báo cáo.xlsx"), m);
        if cfg!(windows) {
            assert_eq!(store.load(r"c:\docs\BÁO CÁO.xlsx"), m, "the same file however the path is written");
        }
        assert!(store.load("C:/Docs/other.xlsx").is_empty());
        assert!(d.join("memory").read_dir().is_ok_and(|mut r| r.next().is_some()));
        store.save("C:/Docs/Báo cáo.xlsx", &Memory::default());
        assert!(store.load("C:/Docs/Báo cáo.xlsx").is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_portable_form_has_no_session_and_bad_input_is_skipped() {
        let m = Memory { messages: vec![msg(Role::User, "hi")], provider: Some("codex".into()), session: Some("t1".into()) };
        let v = m.to_json(true);
        assert!(v.get("session").is_none());
        assert_eq!(Memory::from_json(&v), Memory { session: None, ..m });
        let odd = json!({"messages": [{"role": "robot", "text": "x"}, {"role": "user"}, 5, {"role": "user", "text": "ok"}]});
        assert_eq!(Memory::from_json(&odd).messages, vec![msg(Role::User, "ok")]);
        assert!(Memory::from_json(&json!("nonsense")).is_empty());
    }

    #[test]
    fn a_recap_keeps_questions_and_answers_latest_first_to_go() {
        assert_eq!(recap(&[msg(Role::Error, "x")]), None);
        let r = recap(&[msg(Role::User, "Q1"), msg(Role::Work, "step"), msg(Role::Claude, "A1"), msg(Role::Error, "Stopped.")]).unwrap();
        assert_eq!(r, "User: Q1\n\nAssistant: A1");
        let long: Vec<Msg> = (0..100).map(|k| msg(Role::User, &format!("{k} {}", "x".repeat(500)))).collect();
        let r = recap(&long).unwrap();
        assert!(r.chars().count() <= RECAP_CHARS + 600);
        assert!(r.ends_with(&"x".repeat(500)) && r.contains("User: 99 "));
        assert!(!r.contains("User: 0 "));
        assert!(with_recap(&r, "Next?").ends_with("Next?"));
    }

    /// The questions the stand-in was asked, with the conversation id each continued.
    type Asked = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    /// A pane with a stand-in assistant: it answers "A: <question>" in conversation `s1`, and
    /// fails to continue conversation `gone`, as Claude Code does once a conversation is deleted.
    fn pane(store: &Store, asked: &Asked) -> super::super::Assistant {
        use super::super::{Assistant, Event, Launch, Running};
        let mut a = Assistant::default();
        a.memory = Some(store.clone());
        let ctl = std::sync::Mutex::new(Some(std::sync::mpsc::channel().1));
        a.starter = Some(Box::new(move |_, _| ctl.lock().ok().and_then(|mut r| r.take()).map(|rx| (4567, rx))));
        a.cli = Some(PathBuf::from("craft-cli"));
        let log = asked.clone();
        a.launcher = std::sync::Arc::new(move |l: Launch| {
            if let Ok(mut v) = log.lock() {
                v.push((l.prompt.clone(), l.session.clone()));
            }
            let (tx, rx) = std::sync::mpsc::channel();
            let events = if l.session.as_deref() == Some("gone") {
                vec![Event::Done(Some("No conversation found".into()))]
            } else {
                let q = l.prompt.rsplit("\n\n").next().unwrap_or_default().to_string();
                vec![Event::Session("s1".into()), Event::Answer(format!("A: {q}")), Event::Done(None)]
            };
            for e in events {
                let _ = tx.send(e);
            }
            Ok(Running { events: rx, stop: Box::new(|| {}) })
        });
        a
    }

    fn ask(a: &mut super::super::Assistant, ctx: &egui::Context, q: &str) {
        a.input = q.into();
        a.send(ctx);
        a.poll(ctx);
        a.poll(ctx);
    }

    #[test]
    fn each_file_keeps_its_conversation_across_restarts() {
        let d = scratch("pane");
        let store = Store::new(&d);
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ctx = egui::Context::default();
        let texts = |a: &super::super::Assistant| a.messages.iter().map(|m| m.text.clone()).collect::<Vec<_>>();
        {
            let mut a = pane(&store, &asked);
            // An unsaved document's chat goes with it when it is saved.
            a.follow_document(1, None, None);
            ask(&mut a, &ctx, "Q1");
            a.follow_document(1, Some("C:/Docs/a.file"), None);
            // Another document starts empty, and the first one's chat comes back.
            a.follow_document(2, Some("C:/Docs/b.file"), None);
            assert!(a.messages.is_empty());
            ask(&mut a, &ctx, "Qb");
            a.follow_document(1, Some("C:/Docs/a.file"), None);
            assert_eq!(texts(&a), ["Q1", "A: Q1"]);
        }
        // After a restart: the conversation is continued by its id.
        let mut a = pane(&store, &asked);
        a.follow_document(7, Some("C:/Docs/a.file"), None);
        assert_eq!(texts(&a), ["Q1", "A: Q1"]);
        assert_eq!(a.session.as_deref(), Some("s1"));
        ask(&mut a, &ctx, "Q2");
        assert_eq!(asked.lock().unwrap().last().cloned(), Some(("Q2".into(), Some("s1".into()))));
        // A conversation that is gone: asked again once, with the recap, and nothing shows.
        a.session = Some("gone".into());
        ask(&mut a, &ctx, "Q3");
        let last = asked.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last.1, None);
        assert!(last.0.contains("User: Q1") && last.0.contains("Assistant: A: Q2") && last.0.ends_with("Q3"), "{}", last.0);
        assert_eq!(texts(&a).last().map(String::as_str), Some("A: Q3"));
        assert!(!a.messages.iter().any(|m| m.role == Role::Error));
        // A file that brings its chat (embedded, no id): shown, and recapped on the next question.
        let embedded = Memory { messages: vec![msg(Role::User, "Old"), msg(Role::Claude, "Done")], ..Memory::default() }.to_json(true);
        a.follow_document(9, Some("C:/Docs/c.file"), Some(&embedded));
        assert_eq!(texts(&a), ["Old", "Done"]);
        ask(&mut a, &ctx, "More");
        assert!(asked.lock().unwrap().last().unwrap().0.starts_with("Earlier conversation"));
        // New chat forgets the file's memory.
        a.new_chat();
        assert!(store.load("C:/Docs/c.file").is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
