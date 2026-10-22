//! A stand-in Home Assistant on a loopback port, so the client can be
//! tested without a real one. It answers REST calls through a closure,
//! speaks just enough of the WebSocket API to hand out an entity
//! registry, and records every request it was sent.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};
use tungstenite::Message;

/// What the fake answers a REST call with: a status and a body.
pub type Answer = dyn Fn(&str, &str, &Value) -> (u16, String) + Send + Sync;

pub struct Fake {
    pub url: String,
    /// Every request so far, as "GET /api/states" or
    /// "POST /api/services/light/turn_on {body}".
    pub asked: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    /// A fake that accepts `token`, answers REST calls with `answer`,
    /// and lists `registry` over the WebSocket API; with no registry,
    /// it refuses the WebSocket.
    pub fn start(
        token: &str,
        registry: Option<Value>,
        answer: impl Fn(&str, &str, &Value) -> (u16, String) + Send + Sync + 'static,
    ) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let answer: Arc<Answer> = Arc::new(answer);
        let (token, log) = (token.to_string(), asked.clone());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (token, registry, answer, log) = (token.clone(), registry.clone(), answer.clone(), log.clone());
                thread::spawn(move || serve(stream, &token, registry, &*answer, &log));
            }
        });
        Fake { url, asked }
    }

    pub fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

fn serve(stream: TcpStream, token: &str, registry: Option<Value>, answer: &Answer, log: &Mutex<Vec<String>>) {
    let mut first = [0u8; 64];
    let Ok(n) = stream.peek(&mut first) else { return };
    if first[..n].starts_with(b"GET /api/websocket") {
        return websocket(stream, token, registry);
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
    let (mut length, mut authorized) = (0, false);
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').unwrap_or((&header, ""));
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap_or(0),
            "authorization" => authorized = value.trim() == format!("Bearer {token}"),
            _ => {}
        }
    }
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let entry = if body.is_null() { format!("{method} {path}") } else { format!("{method} {path} {body}") };
    log.lock().unwrap().push(entry);
    let (code, text) = if authorized { answer(&method, &path, &body) } else { (401, "401: Unauthorized".into()) };
    let mut stream = reader.into_inner();
    let _ = write!(
        stream,
        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

fn websocket(mut stream: TcpStream, token: &str, registry: Option<Value>) {
    let Some(registry) = registry else {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let Ok(mut ws) = tungstenite::accept(stream) else { return };
    let send = |ws: &mut tungstenite::WebSocket<TcpStream>, v: Value| ws.send(Message::text(v.to_string()));
    let read = |ws: &mut tungstenite::WebSocket<TcpStream>| -> Option<Value> {
        match ws.read().ok()? {
            Message::Text(t) => serde_json::from_str(&t).ok(),
            _ => None,
        }
    };
    let _ = send(&mut ws, json!({"type": "auth_required"}));
    let Some(auth) = read(&mut ws) else { return };
    if auth["access_token"] != token {
        let _ = send(&mut ws, json!({"type": "auth_invalid", "message": "Invalid access token"}));
        return;
    }
    let _ = send(&mut ws, json!({"type": "auth_ok"}));
    let Some(list) = read(&mut ws) else { return };
    let _ = send(&mut ws, json!({"id": list["id"], "type": "result", "success": true, "result": registry}));
    let _ = ws.read();
}
