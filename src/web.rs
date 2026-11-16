//! The web client. A second port serves the desktop client compiled for the browser (from the folder
//! `scripts/build-web.sh` writes) over HTTP, and lets it talk to this server over a WebSocket, as a
//! browser cannot open the TLS sockets the desktop client uses.
//!
//! Each WebSocket carries the same JSON lines as the desktop protocol and is joined to the normal
//! connection handler, so the page is served exactly like a desktop client. A browser cannot reach the
//! auth service either, so a message of the form `{"auth": AuthRequest}` is passed on to it and its
//! answer comes back as `{"auth": AuthReply}`, or `{"auth_unavailable": why}` when it cannot be
//! reached. The requests only chat servers make are refused.

use std::sync::Arc;

use common::{AuthReply, AuthRequest, ServerMsg};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

use crate::SharedState;

/// The page shown when the web client has not been built yet.
const NOT_BUILT: &str = include_str!("../web/not-built.html");

/// Where the built web client is: VAXELIUM_WEB_DIR, or the folder the build script writes to.
fn web_dir() -> std::path::PathBuf {
    std::env::var("VAXELIUM_WEB_DIR")
        .map(Into::into)
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/web/dist").into())
}

/// The longest request head a browser may send, and the longest line a page may send.
const MAX_HEAD: usize = 16 * 1024;
const MAX_LINE: usize = 64 * 1024;

/// Serves the web client on `addr` until the server stops.
pub async fn serve(
    addr: String,
    state: SharedState,
    tx: broadcast::Sender<ServerMsg>,
    auth_addr: Arc<String>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&addr).await?;
    println!("Vaxelium web client on http://{addr}");
    loop {
        let (socket, peer) = listener.accept().await?;
        let (state, tx, auth_addr) = (state.clone(), tx.clone(), auth_addr.clone());
        tokio::spawn(async move {
            if let Err(e) = handle(socket, state, tx, auth_addr).await {
                eprintln!("web {peer}: {e}");
            }
        });
    }
}

/// One HTTP request: a page file, or the WebSocket the page talks through.
async fn handle(
    mut socket: TcpStream,
    state: SharedState,
    tx: broadcast::Sender<ServerMsg>,
    auth_addr: Arc<String>,
) -> anyhow::Result<()> {
    let head = read_head(&mut socket).await?;
    let mut lines = head.lines();
    let request = lines.next().unwrap_or_default();
    let path = request.split_whitespace().nth(1).unwrap_or("/");
    let header = |name: &str| {
        head.lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    };
    let upgrade = header("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if path == "/ws" && upgrade {
        let key = header("sec-websocket-key").unwrap_or_default();
        let reply = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            derive_accept_key(key.as_bytes())
        );
        socket.write_all(reply.as_bytes()).await?;
        let ws = WebSocketStream::from_raw_socket(socket, Role::Server, None).await;
        return bridge(ws, state, tx, auth_addr).await;
    }
    let path = path.split('?').next().unwrap_or("/");
    // A compressed copy of a large file is sent when the browser accepts it.
    let gzip_ok = header("accept-encoding").is_some_and(|v| v.contains("gzip"));
    let mut encoding = "";
    let mut found = None;
    if gzip_ok {
        if let Some((_, body)) = file(&format!("{path}.gz")).await {
            let kind = file_kind(path);
            encoding = "Content-Encoding: gzip\r\n";
            found = Some((kind, body));
        }
    }
    if found.is_none() {
        found = file(path).await;
    }
    let (status, kind, body) = match found {
        Some((kind, body)) => ("200 OK", kind, body),
        None if path == "/" || path == "/index.html" => {
            ("200 OK", "text/html; charset=utf-8", NOT_BUILT.as_bytes().to_vec())
        }
        None => ("404 Not Found", "text/plain; charset=utf-8", b"Not found".to_vec()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\n{encoding}Content-Length: {}\r\nCache-Control: no-cache\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(&body).await?;
    Ok(())
}

/// A file of the built web client, with its type, or None when there is no such file. Only plain
/// names inside the folder are served.
async fn file(path: &str) -> Option<(&'static str, Vec<u8>)> {
    let name = if path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };
    let safe = name
        .split('/')
        .all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains('\\'));
    if !safe {
        return None;
    }
    let body = tokio::fs::read(web_dir().join(name)).await.ok()?;
    Some((file_kind(name), body))
}

/// The content type of a file, by its name.
fn file_kind(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    }
}

/// The request line and headers, up to the blank line that ends them. The browser sends nothing more
/// before it is answered.
async fn read_head(socket: &mut TcpStream) -> anyhow::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        anyhow::ensure!(head.len() < MAX_HEAD, "request head too long");
        if socket.read(&mut byte).await? == 0 {
            anyhow::bail!("closed before the request was complete");
        }
        head.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// Joins a page's WebSocket to the normal connection handler. Lines the page sends go to the handler
/// (or, for `{"auth": …}`, to the auth service); lines the handler writes go to the page.
async fn bridge(
    ws: WebSocketStream<TcpStream>,
    state: SharedState,
    tx: broadcast::Sender<ServerMsg>,
    auth_addr: Arc<String>,
) -> anyhow::Result<()> {
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    {
        let (state, tx, auth_addr) = (state.clone(), tx.clone(), auth_addr.clone());
        tokio::spawn(async move {
            let _ = crate::handle_connection(theirs, state, tx, auth_addr).await;
        });
    }
    let (from_handler, mut to_handler) = tokio::io::split(ours);
    let (mut ws_out, mut ws_in) = ws.split();
    // Lines for the page, or None once the handler has ended the session.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Option<String>>();

    // Everything for the page goes through one writer. When the handler ends the session (a refused
    // token, say), the socket is closed so the page can start afresh.
    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            let sent = match line {
                Some(line) => ws_out.send(WsMessage::text(line)).await,
                None => {
                    let _ = ws_out.send(WsMessage::Close(None)).await;
                    break;
                }
            };
            if sent.is_err() {
                break;
            }
        }
    });
    {
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(from_handler).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if out_tx.send(Some(line)).is_err() {
                    break;
                }
            }
            let _ = out_tx.send(None);
        });
    }

    while let Some(frame) = ws_in.next().await {
        let text = match frame? {
            WsMessage::Text(text) => text.to_string(),
            WsMessage::Close(_) => break,
            _ => continue,
        };
        if text.len() > MAX_LINE {
            continue;
        }
        if let Some(request) = auth_request(&text) {
            let line = match request {
                Some(request) => match crate::auth_call(&state, &auth_addr, &request).await {
                    Ok(reply) => serde_json::json!({ "auth": reply }),
                    Err(e) => serde_json::json!({ "auth_unavailable": e }),
                },
                None => serde_json::json!({
                    "auth": AuthReply::Error { text: "that request is not available from the web client".into() }
                }),
            }
            .to_string();
            let _ = out_tx.send(Some(line));
            continue;
        }
        // A line break inside the text would split it into two messages for the handler.
        let line = text.replace(['\n', '\r'], " ");
        // The handler may have ended the session already; the page is told by the socket closing.
        if to_handler.write_all(format!("{line}\n").as_bytes()).await.is_err() {
            break;
        }
    }
    // Closing the handler's side ends its connection, as a desktop client leaving would.
    let _ = to_handler.shutdown().await;
    writer.abort();
    Ok(())
}

/// An auth request in a page's line, when it is one: `Some(Some(request))` for one the web client may
/// make, `Some(None)` for one only chat servers may make.
fn auth_request(text: &str) -> Option<Option<AuthRequest>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let auth = value.get("auth")?;
    let request: AuthRequest = serde_json::from_value(auth.clone()).ok()?;
    // These are for chat servers: looking accounts up, recording where they are, and moving old ids.
    Some(match request {
        AuthRequest::Lookup { .. }
        | AuthRequest::Seen { .. }
        | AuthRequest::Locate { .. }
        | AuthRequest::Migrations { .. } => None,
        _ => Some(request),
    })
}

#[cfg(test)]
#[path = "tests/web.rs"]
mod tests;
