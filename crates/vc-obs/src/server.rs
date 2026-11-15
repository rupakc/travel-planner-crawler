//! A tiny HTTP/1.1 endpoint inside each role's container: `GET /metrics` (Prometheus text, for
//! the Ops Agent) and `GET /healthz` (200 healthy / 503 stalled, for `vcrawl health`).
//! Read-only, no request bodies, 8 KiB header cap, 5 s read timeout, `Connection: close`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::health::ProgressTracker;
use crate::metrics::{CONTENT_TYPE, Metrics};

const MAX_HEAD: usize = 8 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);

/// Serves until the task is aborted.
pub async fn serve(
    listener: TcpListener,
    metrics: Arc<Metrics>,
    tracker: Arc<ProgressTracker>,
) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let (m, t) = (metrics.clone(), tracker.clone());
        tokio::spawn(async move {
            let _ = tokio::time::timeout(TIMEOUT, handle(stream, m, t)).await;
        });
    }
}

async fn handle(
    mut stream: TcpStream,
    metrics: Arc<Metrics>,
    tracker: Arc<ProgressTracker>,
) -> std::io::Result<()> {
    let mut head = Vec::with_capacity(512);
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
        if head.len() > MAX_HEAD {
            return respond(&mut stream, 431, "text/plain", "header too large").await;
        }
    }
    let line = String::from_utf8_lossy(&head);
    let mut parts = line.lines().next().unwrap_or("").split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method != "GET" {
        return respond(&mut stream, 405, "text/plain", "method not allowed").await;
    }
    match path {
        "/metrics" => respond(&mut stream, 200, CONTENT_TYPE, &metrics.encode()).await,
        "/healthz" => {
            let body = tracker.snapshot().to_string();
            let status = if tracker.health().is_ok() { 200 } else { 503 };
            respond(&mut stream, status, "application/json", &body).await
        }
        _ => respond(&mut stream, 404, "text/plain", "not found").await,
    }
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}

/// `GET path` on a local endpoint: `(status, body)`.
pub async fn get(addr: SocketAddr, path: &str) -> std::io::Result<(u16, String)> {
    let fut = async {
        let mut s = TcpStream::connect(addr).await?;
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes())
            .await?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).await?;
        let text = String::from_utf8_lossy(&out).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| std::io::Error::other("bad status line"))?;
        let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
        Ok((status, body))
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
}

/// `true` when `/healthz` answers 200 (what `vcrawl health` checks).
pub async fn probe(addr: SocketAddr) -> std::io::Result<bool> {
    Ok(get(addr, "/healthz").await?.0 == 200)
}
