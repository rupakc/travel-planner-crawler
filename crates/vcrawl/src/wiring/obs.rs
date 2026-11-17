//! Observability for a role: tracing, `/metrics` + `/healthz`, heartbeats.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::task::JoinHandle;
use vc_obs::{HeartbeatWriter, Metrics, ProgressTracker};

use crate::Ctx;

/// Installs the JSON/pretty tracing subscriber for `role` (a second call is a no-op: under
/// `vcrawl run` the process installed it once, as `vcrawl`).
pub fn init_tracing(ctx: &Ctx, role: &str) {
    if tracing::dispatcher::has_been_set() {
        return;
    }
    if let Err(e) = vc_obs::init_tracing(&ctx.cfg.obs, role) {
        // Already installed (tests) or a bad filter: fall back to whatever is set.
        eprintln!("vcrawl: tracing not (re)initialised: {e}");
    }
}

/// The metrics registry for a process.
pub fn metrics(ctx_cfg: &vc_config::Config, clock: Arc<dyn vc_core::Clock>) -> Arc<Metrics> {
    Arc::new(Metrics::new(
        clock,
        vc_obs::heartbeat_freshness(&ctx_cfg.obs),
    ))
}

/// The progress tracker (health window `obs.progress_window_s`).
pub fn tracker(
    ctx_cfg: &vc_config::Config,
    clock: Arc<dyn vc_core::Clock>,
) -> Arc<ProgressTracker> {
    Arc::new(ProgressTracker::new(
        clock,
        Duration::from_secs(ctx_cfg.obs.progress_window_s.max(1)),
    ))
}

/// Background tasks serving `/metrics` + `/healthz` and writing heartbeats; aborted on drop.
#[derive(Debug, Default)]
pub struct ObsTasks {
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for ObsTasks {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Starts the metrics/health endpoint (unless `obs.metrics_addr` is empty, or `vcrawl run`
/// serves it for the whole process) and the heartbeat writer for `role`.
pub async fn start(ctx: &Ctx, role: &str, pool: PgPool) -> vc_core::Result<ObsTasks> {
    let mut out = ObsTasks::default();
    let listener = if ctx.shared_obs {
        None
    } else {
        vc_obs::bind_metrics(&ctx.cfg.obs).await?
    };
    if let Some(listener) = listener {
        let (m, t) = (ctx.metrics.clone(), ctx.tracker.clone());
        out.tasks.push(tokio::spawn(async move {
            if let Err(e) = vc_obs::server::serve(listener, m, t).await {
                tracing::warn!(error = %e, "metrics server stopped");
            }
        }));
    }
    let hb = HeartbeatWriter::new(
        pool,
        role,
        &ctx.instance,
        ctx.tracker.clone(),
        ctx.metrics.clone(),
    );
    let every = Duration::from_secs(ctx.cfg.obs.heartbeat_s.max(1));
    out.tasks.push(tokio::spawn(hb.run(every)));
    Ok(out)
}

/// The local address `vcrawl health` probes for `obs.metrics_addr` (`0.0.0.0` → loopback).
pub fn local_metrics_addr(metrics_addr: &str) -> Option<SocketAddr> {
    let a = metrics_addr.trim();
    if a.is_empty() {
        return None;
    }
    let mut addr: SocketAddr = a.parse().ok()?;
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    Some(addr)
}

/// One role's progress health, as `vcrawl run`'s `/healthz` reports it.
#[derive(Debug, Clone)]
pub struct RoleHealth {
    /// The role.
    pub role: String,
    /// Its tracker.
    pub tracker: Arc<ProgressTracker>,
    /// `false` while the role is not running (failed, waiting to be restarted).
    pub up: Arc<std::sync::atomic::AtomicBool>,
}

impl RoleHealth {
    fn ok(&self) -> bool {
        self.up.load(std::sync::atomic::Ordering::Relaxed) && self.tracker.health().is_ok()
    }
}

/// `/healthz` of a multi-role process: 200 when every role is up and not stalled, else 503;
/// the body is `{"healthy": bool, "roles": {role: snapshot + "up"}}`.
pub fn roles_health(roles: &[RoleHealth]) -> (u16, String) {
    let mut map = serde_json::Map::new();
    let mut healthy = true;
    for r in roles {
        let mut snap = r.tracker.snapshot();
        let up = r.up.load(std::sync::atomic::Ordering::Relaxed);
        if let Some(o) = snap.as_object_mut() {
            o.insert("up".into(), up.into());
        }
        healthy &= r.ok();
        map.insert(r.role.clone(), snap);
    }
    let body = serde_json::json!({ "healthy": healthy, "roles": map }).to_string();
    (if healthy { 200 } else { 503 }, body)
}

/// Serves `GET /metrics` (the shared registry: one `crawler_heartbeat{role}` series per role)
/// and `GET /healthz` ([`roles_health`]) until the task is aborted. Same limits as
/// `vc_obs::server`: GET only, 8 KiB head, 5 s per connection, `Connection: close`.
pub async fn serve_roles(
    listener: tokio::net::TcpListener,
    metrics: Arc<Metrics>,
    roles: Arc<Vec<RoleHealth>>,
) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let (m, r) = (metrics.clone(), roles.clone());
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle(stream, m, r)).await;
        });
    }
}

async fn handle(
    mut stream: tokio::net::TcpStream,
    metrics: Arc<Metrics>,
    roles: Arc<Vec<RoleHealth>>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = Vec::with_capacity(512);
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
        if head.len() > 8 * 1024 {
            break;
        }
    }
    let line = String::from_utf8_lossy(&head);
    let mut parts = line.lines().next().unwrap_or("").split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (status, ctype, body) = if head.len() > 8 * 1024 {
        (431, "text/plain", "header too large".to_string())
    } else if method != "GET" {
        (405, "text/plain", "method not allowed".to_string())
    } else {
        match path {
            "/metrics" => (200, vc_obs::metrics::CONTENT_TYPE, metrics.encode()),
            "/healthz" => {
                let (s, b) = roles_health(&roles);
                (s, "application/json", b)
            }
            _ => (404, "text/plain", "not found".to_string()),
        }
    };
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        _ => "Service Unavailable",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await
}
