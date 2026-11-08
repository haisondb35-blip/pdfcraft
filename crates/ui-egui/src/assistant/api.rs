//! Assistants reached over HTTP with the person's own account: any server that speaks the
//! OpenAI-compatible chat API (OpenAI, OpenRouter, LM Studio and others), Google's Gemini through
//! its OpenAI-compatible endpoint, and Ollama, which runs models on this computer (nothing leaves
//! it). Claude Code and Codex are programs that run the conversation themselves; for these the pane
//! does: it starts the app's own MCP server (the one Claude Code gets, connected to this window),
//! gives the model its tools, runs the calls the model asks for and sends back what they returned,
//! until the model answers.
//!
//! What each needs (address, model, key) is kept in `providers.json` in the AI folder on this
//! computer, never in a document and never synced (`craft_sync::mirror::ai_files` leaves it out);
//! a key can come from `OPENAI_API_KEY` / `GEMINI_API_KEY` instead. A conversation continues while
//! the app runs (its messages are held here, by id); after a restart the pane's recap carries it.

use std::path::Path;

use serde_json::{Value, json};

use super::Provider;

/// What one assistant reached over HTTP needs.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// The API's address, up to and without `/chat/completions`.
    pub url: String,
    pub model: String,
    /// The account's key (none for Ollama).
    pub key: String,
}

impl std::fmt::Debug for Config {
    // Never the key: a Debug print can end up in a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("key", &if self.key.is_empty() { "" } else { "…" })
            .finish()
    }
}

/// The file the settings are kept in, in the AI folder.
const FILE: &str = "providers.json";

/// What a provider starts with.
pub fn defaults(p: Provider) -> Config {
    let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).unwrap_or_default();
    match p {
        Provider::Ollama => Config { url: "http://localhost:11434/v1".into(), model: String::new(), key: String::new() },
        Provider::OpenAi => Config { url: "https://api.openai.com/v1".into(), model: "gpt-5-mini".into(), key: env("OPENAI_API_KEY") },
        Provider::Gemini => Config {
            url: "https://generativelanguage.googleapis.com/v1beta/openai".into(),
            model: "gemini-2.5-flash".into(),
            key: env("GEMINI_API_KEY"),
        },
        Provider::Claude | Provider::Codex => Config::default(),
    }
}

/// The saved settings for `p` (its defaults where nothing was saved). `dir`: the AI folder.
pub fn load(dir: Option<&Path>, p: Provider) -> Config {
    let mut c = defaults(p);
    let saved = dir.and_then(|d| std::fs::read(d.join(FILE)).ok()).and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    if let Some(v) = saved.as_ref().and_then(|v| v.get(p.key())) {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        if let Some(u) = s("url") {
            c.url = u;
        }
        if let Some(m) = s("model") {
            c.model = m;
        }
        if let Some(k) = s("key") {
            c.key = k;
        }
    }
    c
}

/// Keeps `c` as `p`'s settings in `dir` (the AI folder).
pub fn save(dir: &Path, p: Provider, c: &Config) -> Result<(), String> {
    let file = dir.join(FILE);
    let mut v =
        std::fs::read(&file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    v[p.key()] = json!({"url": c.url.trim(), "model": c.model.trim(), "key": c.key.trim()});
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, v.to_string()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &file).map_err(|e| format!("{}: {e}", file.display()))
}

/// The tools an MCP server lists, as the chat API takes them (at most 128, the API's limit).
pub fn chat_tools(mcp_tools: &Value) -> Vec<Value> {
    mcp_tools
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| {
                    let name = t.get("name").and_then(Value::as_str)?;
                    let schema =
                        t.get("inputSchema").filter(|s| s.is_object()).cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                    Some(json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": t.get("description").and_then(Value::as_str).unwrap_or_default(),
                            "parameters": schema,
                        }
                    }))
                })
                .take(128)
                .collect()
        })
        .unwrap_or_default()
}

/// The longest tool result sent back to the model.
const MAX_RESULT: usize = 60_000;

/// What a tool call returned, as text for the model: the text parts, a note for each picture,
/// "Error: …" first when the tool failed. Cut to [`MAX_RESULT`] characters.
pub fn result_text(result: &Value) -> String {
    let parts: Vec<String> = result
        .get("content")
        .and_then(Value::as_array)
        .map(|c| {
            c.iter()
                .map(|p| match p.get("type").and_then(Value::as_str) {
                    Some("text") => p.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
                    Some("image") => "[a picture the model can't see here]".to_string(),
                    _ => p.to_string(),
                })
                .collect()
        })
        .unwrap_or_else(|| vec![result.to_string()]);
    let mut text = parts.join("\n");
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        text = format!("Error: {text}");
    }
    if text.chars().count() > MAX_RESULT {
        text = text.chars().take(MAX_RESULT).collect::<String>() + "\n[cut short]";
    }
    text
}

/// One reply of the chat API: what the model said, and the tool calls it asked for (id, name,
/// arguments).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reply {
    pub text: String,
    pub calls: Vec<(String, String, Value)>,
    /// The message as the API sent it, to send back with the conversation.
    pub message: Value,
}

/// Reads a chat API answer.
pub fn parse_reply(v: &Value) -> Result<Reply, String> {
    let message =
        v.pointer("/choices/0/message").filter(|m| m.is_object()).cloned().ok_or_else(|| {
            v.pointer("/error/message").and_then(Value::as_str).map_or_else(|| "the model sent no message".to_string(), str::to_string)
        })?;
    let text = message.get("content").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|c| {
                    let id = c.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                    let f = c.get("function")?;
                    let name = f.get("name").and_then(Value::as_str)?.to_string();
                    // The arguments are JSON in a string (some servers send the object itself).
                    let args = match f.get("arguments") {
                        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                        Some(v @ Value::Object(_)) => v.clone(),
                        _ => json!({}),
                    };
                    Some((id, name, args))
                })
                .collect()
        })
        .unwrap_or_default();
    // Sent back as the API's own message, without fields some servers refuse on input.
    let mut message = message;
    if let Some(m) = message.as_object_mut() {
        m.retain(|k, _| matches!(k.as_str(), "role" | "content" | "tool_calls"));
        m.insert("role".into(), json!("assistant"));
    }
    Ok(Reply { text, calls, message })
}

/// The error a server sent, in a line.
pub fn error_text(status: u16, body: &str) -> String {
    let said = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.pointer("/0/error/message"))
                .or_else(|| v.get("error"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().chars().take(300).collect());
    match status {
        401 | 403 => format!("the key was refused ({status}): {said}"),
        404 => format!("not found ({status}): check the address and the model name. {said}"),
        429 => format!("too many requests or no credit left ({status}): {said}"),
        _ => format!("the server answered {status}: {said}"),
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{launch, models, status};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::{Config, Reply, chat_tools, error_text, parse_reply, result_text};
    use crate::assistant::{Event, Launch, Login, Provider, Running, SYSTEM_PROMPT, describe};

    /// The most rounds of tool calls in one answer.
    const MAX_ROUNDS: usize = 40;
    /// The longest answer read from a server or a line from the MCP server.
    const MAX_BODY: u64 = 32 << 20;
    /// Conversations kept while the app runs, and the most messages kept of each.
    const MAX_CONVERSATIONS: usize = 32;
    const MAX_MESSAGES: usize = 300;

    type Conversations = Mutex<HashMap<String, Vec<Value>>>;

    fn conversations() -> &'static Conversations {
        static C: OnceLock<Conversations> = OnceLock::new();
        C.get_or_init(Conversations::default)
    }

    fn agent(timeout: Duration) -> ureq::Agent {
        // The OS checks certificates, as for updates (craft-update).
        let tls = ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build();
        ureq::Agent::config_builder()
            .tls_config(tls)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .user_agent(concat!("PdfCraft/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent()
    }

    fn read_body(resp: &mut ureq::http::Response<ureq::Body>) -> Result<String, String> {
        let mut text = String::new();
        resp.body_mut().with_config().reader().take(MAX_BODY).read_to_string(&mut text).map_err(|e| e.to_string())?;
        Ok(text)
    }

    /// The host of a URL, for messages (never the path or a key).
    fn host(url: &str) -> &str {
        url.split("://").nth(1).unwrap_or(url).split(['/', '?']).next().unwrap_or(url)
    }

    /// The models Ollama has on this computer (`/api/tags`), from the OpenAI-style address.
    pub fn models(cfg: &Config) -> Result<Vec<String>, String> {
        let base = cfg.url.trim_end_matches('/').trim_end_matches("/v1");
        let url = format!("{base}/api/tags");
        let mut resp = agent(Duration::from_secs(4)).get(url.as_str()).call().map_err(|e| format!("Ollama isn't running at {} ({e})", host(&url)))?;
        let body = read_body(&mut resp)?;
        let v: Value = serde_json::from_str(&body).map_err(|e| format!("Ollama's answer couldn't be read: {e}"))?;
        let Some(list) = v.get("models").and_then(Value::as_array) else { return Ok(Vec::new()) };
        Ok(list.iter().filter_map(|x| x.get("name")?.as_str().map(str::to_string)).collect())
    }

    /// Whether the assistant can answer: Ollama is running and has a model; the others have a key.
    pub fn status(p: Provider, cfg: &Config) -> Login {
        match p {
            Provider::Ollama => match models(cfg) {
                Ok(m) if m.is_empty() => Login::Unknown("Ollama has no models yet: run `ollama pull qwen3`".into()),
                Ok(m) => {
                    let model = if cfg.model.is_empty() { m.first().cloned().unwrap_or_default() } else { cfg.model.clone() };
                    Login::SignedIn(format!("Ollama, {model}"))
                }
                Err(_) => Login::Missing,
            },
            _ if cfg.key.trim().is_empty() => Login::SignedOut,
            _ => Login::SignedIn(if cfg.model.is_empty() { "your key".into() } else { format!("your key, {}", cfg.model) }),
        }
    }

    /// The app's MCP server, spoken to over its standard input and output (JSON-RPC, a line each).
    struct Mcp {
        stdin: ChildStdin,
        out: BufReader<ChildStdout>,
        next: u64,
    }

    impl Mcp {
        fn send(&mut self, msg: &Value) -> Result<(), String> {
            let mut line = msg.to_string();
            line.push('\n');
            self.stdin.write_all(line.as_bytes()).and_then(|()| self.stdin.flush()).map_err(|e| format!("the app's tools stopped ({e})"))
        }

        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            self.next += 1;
            let id = self.next;
            self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
            loop {
                let mut buf = Vec::new();
                let n = (&mut self.out).take(MAX_BODY).read_until(b'\n', &mut buf).map_err(|e| format!("the app's tools stopped ({e})"))?;
                if n == 0 {
                    return Err("the app's tools stopped".into());
                }
                let Ok(v) = serde_json::from_slice::<Value>(&buf) else { continue };
                if v.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(e) = v.get("error") {
                    return Err(e.get("message").and_then(Value::as_str).unwrap_or("the tool failed").to_string());
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    fn post(cfg: &Config, label: &str, messages: &[Value], tools: &[Value]) -> Result<Reply, String> {
        let url = format!("{}/chat/completions", cfg.url.trim().trim_end_matches('/'));
        let mut body = json!({"model": cfg.model.trim(), "messages": messages});
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        let mut req = agent(Duration::from_secs(600)).post(url.as_str()).header("Content-Type", "application/json");
        if !cfg.key.trim().is_empty() {
            req = req.header("Authorization", &format!("Bearer {}", cfg.key.trim()));
        }
        let mut resp = req.send(body.to_string()).map_err(|e| format!("{label}: couldn't reach {} ({e})", host(&url)))?;
        let status = resp.status().as_u16();
        let text = read_body(&mut resp).map_err(|e| format!("{label}: {e}"))?;
        if status != 200 {
            return Err(format!("{label}: {}", error_text(status, &text)));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{label}: the answer couldn't be read ({e})"))?;
        parse_reply(&v).map_err(|e| format!("{label}: {e}"))
    }

    fn new_id() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        format!("api-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
    }

    fn keep(id: &str, history: &[Value]) {
        let mut all = conversations().lock().unwrap_or_else(PoisonError::into_inner);
        if all.len() >= MAX_CONVERSATIONS && !all.contains_key(id) {
            all.clear();
        }
        let start = history.len().saturating_sub(MAX_MESSAGES);
        // The system prompt stays first.
        let mut kept: Vec<Value> = history.first().cloned().into_iter().collect();
        kept.extend(history.iter().skip(start.max(1)).cloned());
        // A tool result whose call was cut off is refused by the API: start at a question.
        while kept.len() > 1 && kept.get(1).and_then(|m| m.get("role")).and_then(Value::as_str) != Some("user") {
            kept.remove(1);
        }
        all.insert(id.to_string(), kept);
    }

    /// One answer: the conversation, the model, the tools, until the model replies without calls.
    fn answer(l: &Launch, cfg: &Config, mcp: &mut Mcp, tx: &Sender<Event>, stop: &AtomicBool) -> Result<(), String> {
        let label = l.provider.label();
        let client = json!({"name": "pdfcraft-assistant", "version": env!("CARGO_PKG_VERSION")});
        mcp.call("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": client}))?;
        mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        let tools = chat_tools(mcp.call("tools/list", json!({}))?.get("tools").unwrap_or(&Value::Null));
        let mut history = match &l.session {
            Some(id) => match conversations().lock().unwrap_or_else(PoisonError::into_inner).get(id) {
                Some(h) => h.clone(),
                // Gone (the app restarted): the pane asks again with a recap.
                None => return Err("the conversation isn't here any more".into()),
            },
            None => vec![json!({"role": "system", "content": SYSTEM_PROMPT})],
        };
        let id = l.session.clone().unwrap_or_else(new_id);
        let _ = tx.send(Event::Session(id.clone()));
        history.push(json!({"role": "user", "content": l.prompt}));
        for _ in 0..MAX_ROUNDS {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let reply = post(cfg, label, &history, &tools)?;
            history.push(reply.message.clone());
            if reply.calls.is_empty() {
                // Small local models sometimes only think, then stop without a word or a tool call.
                if reply.text.is_empty() {
                    return Err(format!(
                        "{label} finished without an answer: the model may be too small to use the app's tools. Try a larger model (Settings)."
                    ));
                }
                keep(&id, &history);
                let _ = tx.send(Event::Answer(reply.text));
                let _ = tx.send(Event::Done(None));
                return Ok(());
            }
            if !reply.text.is_empty() {
                let _ = tx.send(Event::Text(reply.text.clone()));
            }
            for (call_id, name, args) in reply.calls {
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                let _ = tx.send(Event::Step(describe(&name, &args)));
                let content = match mcp.call("tools/call", json!({"name": name, "arguments": args})) {
                    Ok(r) => result_text(&r),
                    Err(e) => format!("Error: {e}"),
                };
                history.push(json!({"role": "tool", "tool_call_id": call_id, "content": content}));
            }
            keep(&id, &history);
        }
        Err(format!("{label} stopped after {MAX_ROUNDS} rounds of tool calls"))
    }

    fn kill(child: &Mutex<Child>) {
        let mut c = child.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = c.kill();
        let _ = c.wait();
    }

    /// Answer `l` with the assistant `cfg` describes, in the background.
    pub fn launch(l: Launch, cfg: Config) -> Result<Running, String> {
        let label = l.provider.label();
        if l.provider != Provider::Ollama && cfg.key.trim().is_empty() {
            return Err(format!("{label} needs your API key: open Settings above the conversation"));
        }
        let mut cfg = cfg;
        if cfg.model.trim().is_empty() {
            if l.provider == Provider::Ollama {
                cfg.model = models(&cfg)?.into_iter().next().ok_or("Ollama has no models yet: run `ollama pull qwen3`")?;
            } else {
                return Err(format!("{label}: choose a model in Settings above the conversation"));
            }
        }
        let mut cmd = Command::new(&l.server.command);
        cmd.args(&l.server.args).envs(l.server.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            // CREATE_NO_WINDOW: no console window flashes up.
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn().map_err(|e| format!("couldn't start the app's tools ({}): {e}", l.server.command.display()))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return Err("couldn't talk to the app's tools".into());
        };
        let child = Arc::new(Mutex::new(child));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (worker_child, worker_stop) = (child.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut mcp = Mcp { stdin, out: BufReader::new(stdout), next: 0 };
            if let Err(e) = answer(&l, &cfg, &mut mcp, &tx, &worker_stop) {
                let _ = tx.send(Event::Done(Some(e)));
            }
            kill(&worker_child);
        });
        Ok(Running {
            events: rx,
            stop: Box::new(move || {
                stop.store(true, Ordering::Release);
                kill(&child);
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_without_showing_the_key() {
        let dir = std::env::temp_dir().join(format!("pdfcraft-api-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let c = Config { url: "https://openrouter.ai/api/v1".into(), model: "some/model".into(), key: "sk-secret".into() };
        save(&dir, Provider::OpenAi, &c).unwrap();
        save(&dir, Provider::Ollama, &Config { model: "qwen3".into(), ..defaults(Provider::Ollama) }).unwrap();
        assert_eq!(load(Some(dir.as_path()), Provider::OpenAi), c);
        assert_eq!(load(Some(dir.as_path()), Provider::Ollama).model, "qwen3");
        assert_eq!(load(Some(dir.as_path()), Provider::Gemini).url, defaults(Provider::Gemini).url, "nothing saved: the defaults");
        assert!(!format!("{c:?}").contains("secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_tools_become_chat_functions() {
        let tools = json!([
            {"name": "get_cell", "description": "Read a cell", "inputSchema": {"type": "object", "properties": {"cell": {"type": "string"}}}},
            {"name": "no_schema"},
            {"description": "no name"}
        ]);
        let f = chat_tools(&tools);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].pointer("/function/name").and_then(Value::as_str), Some("get_cell"));
        assert_eq!(f[0].pointer("/function/parameters/properties/cell/type").and_then(Value::as_str), Some("string"));
        assert_eq!(f[1].pointer("/function/parameters/type").and_then(Value::as_str), Some("object"));
        assert!(chat_tools(&json!("x")).is_empty());
    }

    #[test]
    fn replies_with_and_without_tool_calls_are_read() {
        let v = json!({"choices": [{"message": {"role": "assistant", "content": null, "refusal": null, "tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": "set_cell", "arguments": "{\"cell\":\"A1\",\"input\":\"5\"}"}},
            {"id": "c2", "type": "function", "function": {"name": "get_cell", "arguments": {"cell": "A1"}}},
            {"id": "c3", "type": "function", "function": {"name": "bad", "arguments": "{not json"}}
        ]}}]});
        let r = parse_reply(&v).unwrap();
        assert_eq!(r.text, "");
        assert_eq!(r.calls.len(), 3);
        assert_eq!(r.calls[0], ("c1".into(), "set_cell".into(), json!({"cell": "A1", "input": "5"})));
        assert_eq!(r.calls[1].2, json!({"cell": "A1"}));
        assert_eq!(r.calls[2].2, json!({}));
        assert!(r.message.get("refusal").is_none(), "fields some servers refuse are dropped");
        let done = parse_reply(&json!({"choices": [{"message": {"role": "assistant", "content": " Done. "}}]})).unwrap();
        assert_eq!((done.text.as_str(), done.calls.len()), ("Done.", 0));
        assert_eq!(parse_reply(&json!({"error": {"message": "bad model"}})).unwrap_err(), "bad model");
    }

    #[test]
    fn tool_results_and_errors_read_as_text() {
        let r = json!({"content": [{"type": "text", "text": "A1 = 5"}, {"type": "image", "data": "…"}]});
        assert_eq!(result_text(&r), "A1 = 5\n[a picture the model can't see here]");
        assert!(result_text(&json!({"content": [{"type": "text", "text": "no"}], "isError": true})).starts_with("Error: no"));
        let long = json!({"content": [{"type": "text", "text": "x".repeat(MAX_RESULT + 10)}]});
        assert!(result_text(&long).ends_with("[cut short]"));
        assert!(error_text(401, r#"{"error": {"message": "Incorrect API key"}}"#).contains("key was refused"));
        assert!(error_text(404, "Not Found").contains("check the address"));
        assert!(error_text(500, r#"[{"error": {"message": "boom"}}]"#).ends_with("boom"));
    }
}
