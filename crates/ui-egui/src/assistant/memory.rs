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

/// The names a file's memory is kept under, the one it is saved under first: for a file in a
/// cloud folder (iCloud Drive and the others) its name there, the same on every computer that
/// syncs the folder, so the conversation follows the file; then its path on this computer.
fn keys(path: &str) -> Vec<String> {
    let local = normal(path);
    match craft_sync::cloud::portable_key(&local) {
        Some(cloud) => vec![cloud, local],
        None => vec![local],
    }
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

    fn file(&self, key: &str) -> PathBuf {
        self.dir.join("memory").join(format!("{:016x}.json", fnv(key)))
    }

    /// The memory of the document at `path` (empty when there is none).
    pub fn load(&self, path: &str) -> Memory {
        for key in keys(path) {
            let Ok(b) = std::fs::read(self.file(&key)) else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(&b) else { continue };
            // A hash shared by two paths (vanishingly rare) is not the other file's memory.
            if v.get("path").and_then(Value::as_str).is_some_and(|p| p != key) {
                continue;
            }
            return Memory::from_json(&v);
        }
        Memory::default()
    }

    /// Remembers `m` for `path`; an empty memory forgets it.
    pub fn save(&self, path: &str, m: &Memory) {
        let keys = keys(path);
        let Some(key) = keys.first() else { return };
        // Kept under the first name only: once it is written there, a record under the path on
        // this computer (from before the file's cloud name was used) goes.
        let forget_old = || {
            for old in keys.iter().skip(1) {
                let _ = std::fs::remove_file(self.file(old));
            }
        };
        let f = self.file(key);
        if m.is_empty() {
            let _ = std::fs::remove_file(&f);
            forget_old();
            return;
        }
        let mut v = m.to_json(false);
        v["path"] = json!(key);
        if f.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) {
            // Written beside and renamed, so a crash never leaves half a record.
            let tmp = f.with_extension("json.tmp");
            if std::fs::write(&tmp, v.to_string()).is_ok() && std::fs::rename(&tmp, &f).is_ok() {
                forget_old();
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
        let on = store.as_ref().is_some_and(Store::save_with_file);
        if let Ok(mut s) = self.shelf.lock() {
            s.on = on;
        }
        // The conversations go to the cloud folder too when syncing is on (in any Craft app):
        // a thread of its own mirrors this folder every few minutes.
        if let Some(s) = &store {
            craft_sync::background::start(s.dir().to_path_buf());
        }
        self.memory = store;
        if self.messages.is_empty() && !self.busy() {
            self.doc = None;
        }
    }

    /// Opens this window's channel and lists it for the other Craft apps' assistants, once, when
    /// conversations are remembered (the desktop app): so another app's assistant can read and
    /// work on this one's document when the person asks it to.
    pub fn share(&mut self, ctx: &egui::Context) {
        static TRIED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if self.memory.is_none() || TRIED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let _ = self.prepare(ctx);
    }

    /// Joins the other Craft apps open on this computer: lists this window's server for them, and
    /// adds theirs to `config` (an MCP configuration with this app's one server).
    pub(super) fn join_peers(&self, config: &mut Value) {
        let (Some(store), Some(port)) = (&self.memory, self.port) else { return };
        let Some(servers) = config.get_mut("mcpServers").and_then(Value::as_object_mut) else { return };
        let Some((own, entry)) = servers.iter().next().map(|(k, v)| (k.clone(), v.clone())) else { return };
        store.register(&own, port, &entry);
        for (name, server) in store.peers(&own) {
            servers.entry(name).or_insert(server);
        }
    }
}

/// The conversations a save writes into their files (Save chat with file): kept up to date by
/// the pane, read by the file writer the desktop app hooks in, on another thread.
#[derive(Debug, Default)]
pub struct Shelf {
    /// Save chat with file is on.
    pub on: bool,
    chats: std::collections::HashMap<String, Value>,
    /// The conversation on screen: for a file saved for the first time.
    active: Option<Value>,
}

impl Shelf {
    /// What a save of `path` writes into the file: its conversation, or for a new file the one on
    /// screen. None with the option off (and a file that has one loses it).
    pub fn chat_for(&self, path: &str) -> Option<Value> {
        if !self.on {
            return None;
        }
        self.chats.get(&normal(path)).cloned().or_else(|| if Path::new(path).exists() { None } else { self.active.clone() })
    }

    fn put(&mut self, path: Option<&str>, chat: Option<Value>) {
        if let Some(p) = path {
            match &chat {
                Some(c) => {
                    self.chats.insert(normal(p), c.clone());
                }
                None => {
                    self.chats.remove(&normal(p));
                }
            }
        }
        self.active = chat;
    }
}

impl Store {
    fn settings(&self) -> Value {
        std::fs::read(self.dir.join("settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| json!({}))
    }

    /// Save chat with file, as last chosen in any Craft app (off at first).
    pub fn save_with_file(&self) -> bool {
        self.settings().get("saveWithFile").and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn set_save_with_file(&self, on: bool) {
        let mut v = self.settings();
        v["saveWithFile"] = json!(on);
        if std::fs::create_dir_all(&self.dir).is_ok() {
            let _ = std::fs::write(self.dir.join("settings.json"), v.to_string());
        }
    }

    /// The assistant last chosen in any Craft app (`Provider::key`).
    pub fn provider_choice(&self) -> Option<String> {
        self.settings().get("provider").and_then(Value::as_str).map(str::to_string)
    }

    pub fn set_provider_choice(&self, key: &str) {
        let mut v = self.settings();
        v["provider"] = json!(key);
        if std::fs::create_dir_all(&self.dir).is_ok() {
            let _ = std::fs::write(self.dir.join("settings.json"), v.to_string());
        }
    }
}

impl super::Assistant {
    /// Hands the conversation to the file writer (see [`Shelf`]).
    pub(super) fn shelve(&self) {
        let path = self.doc.as_ref().and_then(|(_, p)| p.as_deref());
        if let Ok(mut s) = self.shelf.lock() {
            s.put(path, self.portable());
        }
    }

    /// Whether this app can save the conversation in its files (it reads them back too).
    pub fn can_save_with_file(&self) -> bool {
        self.memory.is_some() && self.embedded_reader.is_some()
    }

    pub fn save_with_file(&self) -> bool {
        self.shelf.lock().is_ok_and(|s| s.on)
    }

    /// Turns Save chat with file on or off, for every Craft app.
    pub fn set_save_with_file(&mut self, on: bool) {
        if let Some(store) = &self.memory {
            store.set_save_with_file(on);
        }
        if let Ok(mut s) = self.shelf.lock() {
            s.on = on;
        }
        self.shelve();
    }
}

/// How long a listed app has to answer before it counts as closed.
const PEER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

impl Store {
    fn apps(&self) -> PathBuf {
        self.dir.join("apps")
    }

    /// Lists this window's MCP server (`name`, e.g. "cadcraft") for the other Craft apps.
    pub fn register(&self, name: &str, port: u16, server: &Value) {
        let dir = self.apps();
        if std::fs::create_dir_all(&dir).is_ok() {
            let f = dir.join(format!("{name}-{}.json", std::process::id()));
            let _ = std::fs::write(f, json!({"name": name, "port": port, "server": server}).to_string());
        }
    }

    /// Takes this process's windows off the list (the app is closing).
    pub fn leave(&self) {
        let suffix = format!("-{}.json", std::process::id());
        if let Ok(rd) = std::fs::read_dir(self.apps()) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().ends_with(&suffix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    /// The other Craft apps open now (not `own`): each one's name and MCP server. One that no
    /// longer answers is taken off the list.
    pub fn peers(&self, own: &str) -> Vec<(String, Value)> {
        let mut out: Vec<(String, Value)> = Vec::new();
        let Ok(rd) = std::fs::read_dir(self.apps()) else { return out };
        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        files.sort();
        for f in files {
            let Some(v) = std::fs::read(&f).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { continue };
            let (Some(name), Some(port), Some(server)) =
                (v.get("name").and_then(Value::as_str), v.get("port").and_then(Value::as_u64), v.get("server"))
            else {
                continue;
            };
            let Ok(port) = u16::try_from(port) else { continue };
            if name == own || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                continue;
            }
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            if std::net::TcpStream::connect_timeout(&addr, PEER_TIMEOUT).is_err() {
                let _ = std::fs::remove_file(&f);
                continue;
            }
            // Two windows of one app: `cadcraft`, `cadcraft_2`…
            let mut key = name.to_string();
            let mut k = 2;
            while out.iter().any(|(n, _)| *n == key) {
                key = format!("{name}_{k}");
                k += 1;
            }
            out.push((key, server.clone()));
        }
        out
    }
}

/// The servers an MCP configuration file names.
fn servers_in(config: &Path) -> Vec<(String, Value)> {
    std::fs::read(config)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("mcpServers").and_then(Value::as_object).map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()))
        .unwrap_or_default()
}

/// Claude Code's `--allowedTools`: this app's tools and the other Craft apps' in the
/// configuration.
pub fn allow_peers(args: &mut [String], config: &Path) {
    let names = servers_in(config);
    if names.len() < 2 {
        return;
    }
    if let Some(a) = args.iter_mut().find(|a| a.starts_with("mcp__")) {
        // This app's own tools first, then the others'.
        let own = a.clone();
        let mut all = vec![own.clone()];
        all.extend(names.iter().map(|(n, _)| format!("mcp__{n}")).filter(|n| *n != own));
        *a = all.join(",");
    }
}

/// Codex's `-c` settings for the other Craft apps' servers in the configuration (not `own`'s).
pub fn codex_peer_args(config: &Path, own: &str) -> Vec<String> {
    let mut a = Vec::new();
    for (name, s) in servers_in(config) {
        if name == own {
            continue;
        }
        let Some(command) = s.get("command").and_then(Value::as_str) else { continue };
        let args: Vec<String> =
            s.get("args").and_then(Value::as_array).map(|x| x.iter().filter_map(|v| v.as_str().map(super::toml_str)).collect()).unwrap_or_default();
        a.push("-c".into());
        a.push(format!("mcp_servers.{name}.command={}", super::toml_str(command)));
        a.push("-c".into());
        a.push(format!("mcp_servers.{name}.args=[{}]", args.join(",")));
        a.push("-c".into());
        a.push(format!("mcp_servers.{name}.default_tools_approval_mode=\"approve\""));
        if let Some(env) = s.get("env").and_then(Value::as_object).filter(|e| !e.is_empty()) {
            let pairs: Vec<String> = env.iter().filter_map(|(k, v)| Some(format!("{k}={}", super::toml_str(v.as_str()?)))).collect();
            a.push("-c".into());
            a.push(format!("mcp_servers.{name}.env={{{}}}", pairs.join(",")));
        }
    }
    a
}

/// Added to the assistant's instructions: the other Craft apps may be there too (each app's
/// `SYSTEM_PROMPT` carries a copy of this text).
#[allow(dead_code)]
pub const PEERS_NOTE: &str = " Other Craft apps open on this computer (GridCraft, WordCraft, DeckCraft, PdfCraft, PhotoCraft, VectorCraft, FilmCraft, CadCraft, FigCraft, LightCraft, EffectCraft) may be connected too, as tools named after the app: \
use them when the user asks for another app's data (for example, read the drawing open in CadCraft to build from it), and don't change another app's document unless asked.";

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
    fn the_assistant_chosen_is_remembered_beside_save_with_file() {
        let d = scratch("provider");
        let s = Store::new(&d);
        assert_eq!(s.provider_choice(), None);
        s.set_save_with_file(true);
        s.set_provider_choice("ollama");
        assert_eq!(s.provider_choice().as_deref(), Some("ollama"));
        assert!(s.save_with_file(), "one choice doesn't lose the other");
        let _ = std::fs::remove_dir_all(&d);
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
    fn open_craft_apps_find_each_other() {
        let d = scratch("peers");
        let store = Store::new(&d);
        let live = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live_port = live.local_addr().unwrap().port();
        let dead_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let server = |app: &str| json!({"type": "stdio", "command": format!("C:/ArtCraft/{app}-cli.exe"), "args": ["mcp", "--connect", "127.0.0.1:1"], "env": {"TOKEN": "t\"1"}});
        store.register("cadcraft", live_port, &server("cadcraft"));
        std::fs::write(d.join("apps/wordcraft-1.json"), json!({"name": "wordcraft", "port": dead_port, "server": server("wordcraft")}).to_string())
            .unwrap();
        std::fs::write(d.join("apps/bad-2.json"), json!({"name": "a b", "port": live_port, "server": {}}).to_string()).unwrap();
        let peers = store.peers("gridcraft");
        assert_eq!(peers.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["cadcraft"]);
        assert!(!d.join("apps/wordcraft-1.json").exists(), "a closed app leaves the list");
        assert!(store.peers("cadcraft").is_empty(), "an app is not its own peer");
        // The configuration with the peer in it: Claude may use both apps' tools, Codex gets both.
        let config = d.join("mcp.json");
        std::fs::write(&config, json!({"mcpServers": {"gridcraft": server("gridcraft"), "cadcraft": peers[0].1}}).to_string()).unwrap();
        let mut args: Vec<String> = ["-p", "--allowedTools", "mcp__gridcraft"].map(String::from).to_vec();
        allow_peers(&mut args, &config);
        assert_eq!(args[2], "mcp__gridcraft,mcp__cadcraft");
        let codex = codex_peer_args(&config, "gridcraft").join(" ");
        assert!(
            codex.contains("mcp_servers.cadcraft.command=\"C:/ArtCraft/cadcraft-cli.exe\"") && !codex.contains("mcp_servers.gridcraft"),
            "{codex}"
        );
        assert!(codex.contains(r#"mcp_servers.cadcraft.env={TOKEN="t\"1"}"#), "{codex}");
        store.leave();
        assert!(store.peers("gridcraft").is_empty());
        drop(live);
        let _ = std::fs::remove_dir_all(&d);
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
        // The channel counts as open (each app starts it its own way).
        a.port = Some(4567);
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
