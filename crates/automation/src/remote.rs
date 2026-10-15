//! The tools on a running app (`pdfcraft-cli mcp --app PORT`): every call goes to the app's UI
//! control channel as `ui.tool`, and the app runs it on its own session, so the tools act on the
//! documents its window shows (the Claude pane uses this). The channel is the loopback one
//! `pdfcraft --control` opens; its token comes from `PDFCRAFT_CONTROL_TOKEN`.
//!
//! [`encode`] and [`decode`] carry a tool's result over the channel; [`error_text`] and
//! [`parse_error`] its error, keeping the kind (unknown tool, invalid arguments, failed).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::{Content, Result, ToolError};

/// A tool may run for minutes (recognising text in a long scan).
const REPLY_TIMEOUT: Duration = Duration::from_secs(600);
/// The longest reply read (a page rendered at a large size, as base64).
const MAX_REPLY: u64 = 256 << 20;

/// A running PdfCraft, reached through its control channel.
pub struct RemoteApp {
    port: u16,
    token: String,
    conn: Option<(BufReader<TcpStream>, TcpStream)>,
    next_id: u64,
}

impl RemoteApp {
    pub fn new(port: u16, token: String) -> RemoteApp {
        RemoteApp { port, token, conn: None, next_id: 1 }
    }

    fn connect(&mut self) -> std::result::Result<(), String> {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).map_err(|e| format!("can't reach PdfCraft on port {}: {e}", self.port))?;
        stream.set_read_timeout(Some(REPLY_TIMEOUT)).map_err(|e| e.to_string())?;
        let read = stream.try_clone().map_err(|e| e.to_string())?;
        self.conn = Some((BufReader::new(read), stream));
        let token = self.token.clone();
        self.request("auth", json!({ "token": token }))?;
        Ok(())
    }

    /// One JSON-RPC request on the open connection.
    fn request(&mut self, method: &str, params: Value) -> std::result::Result<Value, String> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let Some((reader, writer)) = self.conn.as_mut() else { return Err("not connected".into()) };
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        writer.write_all(line.as_bytes()).and_then(|_| writer.write_all(b"\n")).and_then(|_| writer.flush()).map_err(|e| e.to_string())?;
        let mut reply = String::new();
        let n = reader.by_ref().take(MAX_REPLY).read_line(&mut reply).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("PdfCraft closed the connection".into());
        }
        let v: Value = serde_json::from_str(reply.trim()).map_err(|e| format!("bad reply from PdfCraft: {e}"))?;
        match v.get("error") {
            Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string()),
            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        }
    }

    /// Run tool `name` in the app.
    pub fn call(&mut self, name: &str, args: &Value) -> Result<Vec<Content>> {
        let params = json!({ "name": name, "arguments": args });
        let mut tries = 0;
        loop {
            if self.conn.is_none()
                && let Err(e) = self.connect()
            {
                self.conn = None;
                return Err(ToolError::Failed(e));
            }
            match self.request("ui.tool", params.clone()) {
                Ok(v) => return decode(&v),
                // A connection the app dropped (it restarted): one more try on a fresh one.
                Err(e) if tries == 0 && (e.contains("closed") || e.contains("os error")) => {
                    self.conn = None;
                    tries += 1;
                }
                Err(e) => return Err(parse_error(&e)),
            }
        }
    }
}

/// A tool's result for the channel: `{content: [{json} | {png, width, height}]}`.
pub fn encode(content: &[Content]) -> Value {
    let items: Vec<Value> = content
        .iter()
        .map(|c| match c {
            Content::Json(v) => json!({ "json": v }),
            Content::Png { data, width, height } => {
                json!({ "png": base64::engine::general_purpose::STANDARD.encode(data), "width": width, "height": height })
            }
        })
        .collect();
    json!({ "content": items })
}

/// The tool result [`encode`] made.
pub fn decode(v: &Value) -> Result<Vec<Content>> {
    let items = v.get("content").and_then(Value::as_array).ok_or_else(|| ToolError::Failed("PdfCraft sent no tool result".into()))?;
    items
        .iter()
        .map(|item| {
            if let Some(j) = item.get("json") {
                return Ok(Content::Json(j.clone()));
            }
            let png = item.get("png").and_then(Value::as_str).ok_or_else(|| ToolError::Failed("PdfCraft sent an unknown result".into()))?;
            let data =
                base64::engine::general_purpose::STANDARD.decode(png).map_err(|e| ToolError::Failed(format!("bad image from PdfCraft: {e}")))?;
            let dim = |k: &str| item.get(k).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
            Ok(Content::Png { data, width: dim("width"), height: dim("height") })
        })
        .collect()
}

/// A tool error as the channel's error message, its kind first.
pub fn error_text(e: &ToolError) -> String {
    match e {
        ToolError::UnknownTool(t) => format!("unknown_tool: {t}"),
        ToolError::InvalidArgs(m) => format!("invalid_args: {m}"),
        ToolError::Failed(m) => format!("failed: {m}"),
    }
}

/// The tool error [`error_text`] made (anything else failed).
pub fn parse_error(text: &str) -> ToolError {
    if let Some(t) = text.strip_prefix("unknown_tool: ") {
        ToolError::UnknownTool(t.into())
    } else if let Some(m) = text.strip_prefix("invalid_args: ") {
        ToolError::InvalidArgs(m.into())
    } else {
        ToolError::Failed(text.strip_prefix("failed: ").unwrap_or(text).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_and_errors_cross_the_channel_intact() {
        let content = vec![Content::Json(json!({"pages": 3})), Content::Png { data: vec![137, 80, 78, 71], width: 2, height: 1 }];
        assert_eq!(decode(&encode(&content)).unwrap(), content);
        for e in [ToolError::UnknownTool("nope".into()), ToolError::InvalidArgs("doc is required".into()), ToolError::Failed("no such page".into())] {
            assert_eq!(parse_error(&error_text(&e)), e);
        }
        assert_eq!(parse_error("the app has closed"), ToolError::Failed("the app has closed".into()));
        assert!(decode(&json!({})).is_err() && decode(&json!({"content": [{"what": 1}]})).is_err());
    }

    /// The client against a stand-in app: it authenticates, sends `ui.tool` and reads the result.
    #[test]
    fn calls_reach_the_app_with_the_token() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut out = stream.try_clone().unwrap();
            let mut seen = Vec::new();
            for line in BufReader::new(stream).lines().take(3) {
                let msg: Value = serde_json::from_str(&line.unwrap()).unwrap();
                let id = msg["id"].clone();
                let reply = match msg["method"].as_str().unwrap() {
                    "auth" if msg["params"]["token"] == "t0k" => json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}),
                    "ui.tool" if msg["params"]["name"] == "doc_list" => {
                        json!({"jsonrpc": "2.0", "id": id, "result": encode(&[Content::Json(json!({"documents": []}))])})
                    }
                    _ => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "invalid_args: doc is required"}}),
                };
                seen.push(msg["method"].as_str().unwrap().to_string());
                writeln!(out, "{reply}").unwrap();
            }
            seen
        });
        let mut r = RemoteApp::new(port, "t0k".into());
        assert_eq!(r.call("doc_list", &json!({})).unwrap(), vec![Content::Json(json!({"documents": []}))]);
        assert_eq!(r.call("doc_info", &json!({})).unwrap_err(), ToolError::InvalidArgs("doc is required".into()));
        assert_eq!(app.join().unwrap(), vec!["auth", "ui.tool", "ui.tool"]);
        let mut gone = RemoteApp::new(1, "x".into());
        assert!(matches!(gone.call("doc_list", &json!({})), Err(ToolError::Failed(_))));
    }
}
