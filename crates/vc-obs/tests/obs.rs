//! vc-obs: log allowlist, progress health, alert metric names, heartbeat, HTTP endpoint.

use std::collections::BTreeSet;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use testkit::FakeClock;
use tracing::{info, info_span, warn};
use tracing_subscriber::layer::SubscriberExt;
use vc_obs::{ALERT_METRICS, Health, Metrics, ProgressTracker};

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf {
        self.clone()
    }
}

impl Buf {
    fn lines(&self) -> Vec<serde_json::Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("every line is JSON"))
            .collect()
    }
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

#[test]
fn log_fields_allowlisted_no_body() {
    let buf = Buf::default();
    let subscriber =
        tracing_subscriber::registry().with(vc_obs::logging::json_layer("fetcher", buf.clone()));
    tracing::subscriber::with_default(subscriber, || {
        let span = info_span!(
            "claim",
            key = "example.org",
            epoch = 42i64,
            page_html = "<html>SPAN SECRET</html>"
        );
        let _g = span.enter();
        info!(
            url_key = "ab12cd",
            body = "<html>SECRET BODY</html>",
            token = "tok-123",
            content = "SECRET CONTENT",
            status = 200u16,
            "fetched"
        );
        warn!(
            authorization = "Bearer xyz",
            error_class = "policy",
            "refused"
        );
        info!(source_id = "wikivoyage", "{}", "x".repeat(5000));
    });
    let text = buf.text();
    for secret in [
        "SECRET",
        "tok-123",
        "Bearer",
        "\"body\"",
        "\"token\"",
        "\"content\"",
        "page_html",
        "authorization",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    let lines = buf.lines();
    assert_eq!(lines.len(), 3);
    let l = &lines[0];
    assert_eq!(l["severity"], "INFO");
    assert_eq!(l["message"], "fetched");
    assert_eq!(l["role"], "fetcher");
    assert_eq!(l["key"], "example.org");
    assert_eq!(l["epoch"], 42);
    assert_eq!(l["url_key"], "ab12cd");
    assert_eq!(l["status"], 200);
    assert!(l["timestamp"].is_string());
    assert_eq!(lines[1]["severity"], "WARNING");
    assert_eq!(lines[1]["error_class"], "policy");
    // A long message is cut, so a formatted page can't reach the logs whole.
    let msg = lines[2]["message"].as_str().unwrap();
    assert!(msg.chars().count() <= vc_obs::logging::MAX_MESSAGE_CHARS + 1);
    assert_eq!(lines[2]["source_id"], "wikivoyage");
}

#[test]
fn health_fails_without_progress() {
    let clock = FakeClock::default();
    let p = ProgressTracker::new(Arc::new(clock.clone()), Duration::from_secs(600));
    // Idle with no due work: healthy however long it lasts.
    clock.advance(Duration::from_secs(7200));
    assert_eq!(p.health(), Health::Idle);
    assert!(p.health().is_ok());

    // Work becomes due: the window starts now, not at the last progress.
    p.set_work_due(true);
    assert!(p.health().is_ok());
    clock.advance(Duration::from_secs(599));
    assert!(p.health().is_ok());
    clock.advance(Duration::from_secs(2));
    let h = p.health();
    assert!(!h.is_ok(), "{h:?}");
    assert!(matches!(h, Health::Stalled { since_s } if since_s >= 601));

    // One completion makes it healthy again.
    p.record_progress(1);
    assert_eq!(p.health(), Health::Progressing);
    clock.advance(Duration::from_secs(300));
    assert!(p.health().is_ok());
    clock.advance(Duration::from_secs(301));
    assert!(!p.health().is_ok());

    // No due work → healthy again.
    p.set_work_due(false);
    assert!(p.health().is_ok());
    assert_eq!(p.snapshot()["completed"], 1);
}

fn ident(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_ascii_lowercase() || *c == '_')
        .collect()
}

/// Every `custom.googleapis.com/crawler/*` metric the alert policies and the infrastructure
/// README name.
fn referenced_metrics() -> BTreeSet<String> {
    const PREFIX: &str = "custom.googleapis.com/crawler/";
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infrastructure");
    let mut out = BTreeSet::new();
    let readme = std::fs::read_to_string(root.join("README.md")).unwrap();
    // The README lists them as `custom.googleapis.com/crawler/…` followed by bare names.
    let list = readme
        .split("custom.googleapis.com/crawler/…")
        .nth(1)
        .expect("README metric list")
        .split("Disk usage")
        .next()
        .unwrap();
    // The split point is inside the `…` code span, so code spans are the even parts.
    for (i, part) in list.split('`').enumerate() {
        if i > 0 && i % 2 == 0 && !part.is_empty() && ident(part) == part {
            out.insert(part.to_string());
        }
    }
    out.remove("gce_instance");
    out.remove("role");
    let mut texts = vec![readme.clone()];
    for e in std::fs::read_dir(root.join("monitoring")).unwrap() {
        texts.push(std::fs::read_to_string(e.unwrap().path()).unwrap());
    }
    for t in &texts {
        for part in t.split(PREFIX).skip(1) {
            let name = ident(part);
            if !name.is_empty() {
                out.insert(name);
            }
        }
    }
    out
}

/// Go-live review 1: a gauge exported only once known can never cross a threshold while it
/// is missing, so each of these has an absence condition in some policy.
#[test]
fn exported_when_known_gauges_alert_on_absence() {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infrastructure/monitoring");
    let mut absent = BTreeSet::new();
    for e in std::fs::read_dir(root).unwrap() {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(e.unwrap().path()).unwrap()).unwrap();
        for c in v["conditions"].as_array().unwrap() {
            if let Some(f) = c
                .pointer("/conditionAbsent/filter")
                .and_then(|f| f.as_str())
            {
                let name = f
                    .split("custom.googleapis.com/crawler/")
                    .nth(1)
                    .map(ident)
                    .unwrap_or_default();
                absent.insert(name);
            }
        }
    }
    for name in [
        "heartbeat",
        "wal_archive_age_seconds",
        "base_backup_age_seconds",
        "llm_month_spend_ratio",
        "tls_cert_days_left",
        "deploy_drift_seconds",
        "backup_restore_age_seconds",
    ] {
        assert!(
            absent.contains(name),
            "no absence condition for {name}: {absent:?}"
        );
    }
}

#[test]
fn alert_metric_names_match_monitoring_policies() {
    let referenced = referenced_metrics();
    assert!(referenced.len() >= 11, "{referenced:?}");
    let ours: BTreeSet<String> = ALERT_METRICS
        .iter()
        .map(|m| {
            m.cloud_type
                .trim_start_matches("custom.googleapis.com/crawler/")
                .to_string()
        })
        .collect();
    assert_eq!(ours, referenced);

    // Every one is exposed under its Prometheus name and maps back to the policy's type.
    let clock = FakeClock::default();
    let m = Metrics::new(Arc::new(clock.clone()), Duration::from_secs(180));
    m.heartbeat("fetcher");
    // Gauges are exported once set (a role exports only what it owns).
    m.due_urls.set(1);
    m.backpressure.set(0);
    m.wal_archive_age_seconds.set(60.0);
    m.base_backup_age_seconds.set(3600.0);
    m.restore_drill_ok.set(1);
    m.restore_drill_age_seconds.set(86400.0);
    m.llm_month_spend_ratio.set(0.1);
    m.parked_keys_ratio.set(0.0);
    m.tls_cert_days_left.set(700.0);
    m.llm_config_invalid.set(0);
    m.deploy_drift_seconds.set(0.0);
    m.host_units_failed.set(0);
    m.backup_restore_age_seconds.set(3600.0);
    let text = m.encode();
    for a in ALERT_METRICS {
        assert!(
            text.contains(&format!("# TYPE {} ", a.prometheus_name)),
            "{} missing in\n{text}",
            a.prometheus_name
        );
        assert_eq!(
            vc_obs::metrics::cloud_monitoring_type(a.prometheus_name).as_deref(),
            Some(a.cloud_type)
        );
    }
    assert_eq!(
        vc_obs::metrics::cloud_monitoring_type("crawler_captures_committed_total").as_deref(),
        Some("custom.googleapis.com/crawler/captures_committed")
    );
    assert_eq!(
        vc_obs::metrics::cloud_monitoring_type("http_requests"),
        None
    );
}

#[test]
fn metrics_values_and_heartbeat_absence() {
    let clock = FakeClock::default();
    let m = Metrics::new(Arc::new(clock.clone()), Duration::from_secs(180));
    m.captures_committed.inc_by(3);
    m.due_urls.set(17);
    m.backpressure.set(1);
    m.parked_keys_ratio.set(0.125);
    m.heartbeat("fetcher");
    let text = m.encode();
    assert!(
        text.contains("crawler_captures_committed_total 3"),
        "{text}"
    );
    assert!(text.contains("crawler_due_urls 17"));
    assert!(text.contains("crawler_backpressure 1"));
    assert!(text.contains("crawler_parked_keys_ratio 0.125"));
    assert!(text.contains("crawler_heartbeat{role=\"fetcher\"} 1"));
    // A heartbeat that stopped disappears, so the absence alert fires.
    clock.advance(Duration::from_secs(181));
    let text = m.encode();
    assert!(!text.contains("crawler_heartbeat{"), "{text}");
    m.heartbeat("fetcher");
    assert!(m.encode().contains("crawler_heartbeat{role=\"fetcher\"} 1"));
}

#[tokio::test]
async fn heartbeat_writer_upserts_row() {
    let pool = testkit::pg().await;
    let clock = FakeClock::default();
    let tracker = Arc::new(ProgressTracker::new(
        Arc::new(clock.clone()),
        Duration::from_secs(600),
    ));
    let metrics = Arc::new(Metrics::new(
        Arc::new(clock.clone()),
        Duration::from_secs(180),
    ));
    let hb = vc_obs::HeartbeatWriter::new(
        pool.clone(),
        "processor",
        "inst-1",
        tracker.clone(),
        metrics.clone(),
    );
    hb.beat_once().await.unwrap();
    tracker.record_progress(5);
    hb.beat_once().await.unwrap();
    let rows: Vec<(String, String, serde_json::Value)> =
        sqlx::query_as("SELECT role, instance, progress FROM heartbeats")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "processor");
    assert_eq!(rows[0].1, "inst-1");
    assert_eq!(rows[0].2["completed"], 5);
    assert_eq!(rows[0].2["healthy"], true);
    assert!(
        metrics
            .encode()
            .contains("crawler_heartbeat{role=\"processor\"} 1")
    );
}

#[tokio::test]
async fn http_endpoint_serves_metrics_and_health() {
    let clock = FakeClock::default();
    let tracker = Arc::new(ProgressTracker::new(
        Arc::new(clock.clone()),
        Duration::from_secs(600),
    ));
    let metrics = Arc::new(Metrics::new(
        Arc::new(clock.clone()),
        Duration::from_secs(180),
    ));
    metrics.due_urls.set(4);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(vc_obs::server::serve(
        listener,
        metrics.clone(),
        tracker.clone(),
    ));

    let (status, body) = vc_obs::server::get(addr, "/metrics").await.unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("crawler_due_urls 4"));
    assert!(vc_obs::server::probe(addr).await.unwrap());

    tracker.set_work_due(true);
    clock.advance(Duration::from_secs(601));
    let (status, _) = vc_obs::server::get(addr, "/healthz").await.unwrap();
    assert_eq!(status, 503);
    assert!(!vc_obs::server::probe(addr).await.unwrap());

    let (status, _) = vc_obs::server::get(addr, "/nope").await.unwrap();
    assert_eq!(status, 404);
    server.abort();
}

/// Go-live review 1: the four alert gauges no role set (WAL archive age, base backup age,
/// LLM month spend, TLS days left) and the new host gauges are exported from Postgres, and
/// withdrawn (so the absence condition fires) when their source is missing or stale.
#[tokio::test]
async fn ops_gauges_from_postgres() {
    use chrono::{Duration as CDuration, Utc};
    use vc_obs::ops::{OpsInputs, export_ops};

    let pool = testkit::pg().await;
    let m = Metrics::new(Arc::new(vc_core::SystemClock), Duration::from_secs(180));
    let now = Utc::now();
    let close = |a: Option<f64>, b: f64| a.is_some_and(|a| (a - b).abs() < 1.0);

    // Nothing recorded yet: only what is known is exported.
    export_ops(&pool, &m, &OpsInputs::default(), now)
        .await
        .unwrap();
    assert_eq!(
        m.wal_archive_age_seconds.get(),
        None,
        "testkit has no archiving"
    );
    assert_eq!(m.base_backup_age_seconds.get(), None);
    assert_eq!(m.backup_restore_age_seconds.get(), None);
    assert_eq!(m.deploy_drift_seconds.get(), None);
    assert_eq!(m.tls_cert_days_left.get(), None);
    assert_eq!(m.host_units_failed.get(), Some(0));
    assert_eq!(m.llm_month_spend_ratio.get(), Some(0.0));
    assert_eq!(m.llm_config_invalid.get(), Some(0));
    let text = m.encode();
    assert!(!text.contains("crawler_base_backup_age_seconds "), "{text}");

    let iso = |d: chrono::DateTime<Utc>| d.to_rfc3339();
    let tls_end = (now + CDuration::days(20))
        .format("%b %e %H:%M:%S %Y GMT")
        .to_string();
    let rows = [
        (
            "unit:voyager-crawler-basebackup.service",
            now,
            serde_json::json!({"failed": false, "last_ok_at": iso(now - CDuration::hours(1))}),
        ),
        (
            "unit:voyager-crawler-restore-drill.service",
            now,
            serde_json::json!({"failed": true, "last_ok_at": iso(now - CDuration::days(8))}),
        ),
        (
            "unit:voyager-crawler-deploy.service",
            now,
            serde_json::json!({"failed": false, "last_ok_at": iso(now)}),
        ),
        (
            "deploy",
            now - CDuration::minutes(1),
            serde_json::json!({"running": "sha256:a", "prod": "sha256:b",
                "drift_since": iso(now - CDuration::seconds(2000)), "tls_not_after": tls_end}),
        ),
    ];
    for (instance, beat, progress) in rows {
        sqlx::query(
            "INSERT INTO heartbeats (role, instance, last_beat, progress) VALUES ('host', $1, $2, $3)",
        )
        .bind(instance)
        .bind(beat)
        .bind(progress)
        .execute(&pool)
        .await
        .unwrap();
    }
    // A maintenance-owned row with failed=true must not count as a host unit.
    sqlx::query(
        "INSERT INTO heartbeats (role, instance, progress) VALUES ('maintenance', 'unit:x', '{\"failed\": true}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO llm_budget (period, cap, spent) VALUES ($1, 30, 27)")
        .bind(now.format("%Y-%m").to_string())
        .execute(&pool)
        .await
        .unwrap();

    let forced = OpsInputs {
        llm_forced_off: Some("llm.enabled requires VC_PROJECT_CAP_USD above 0".into()),
    };
    export_ops(&pool, &m, &forced, now).await.unwrap();
    assert!(close(m.base_backup_age_seconds.get(), 3600.0));
    assert!(close(m.backup_restore_age_seconds.get(), 8.0 * 86_400.0));
    assert_eq!(m.host_units_failed.get(), Some(1));
    assert!(close(m.deploy_drift_seconds.get(), 2000.0));
    assert!(close(m.tls_cert_days_left.get(), 20.0));
    assert!(close(m.llm_month_spend_ratio.get(), 0.9));
    assert_eq!(m.llm_config_invalid.get(), Some(1));
    let text = m.encode();
    for name in [
        "crawler_base_backup_age_seconds ",
        "crawler_tls_cert_days_left ",
        "crawler_llm_month_spend_ratio ",
        "crawler_deploy_drift_seconds ",
        "crawler_host_units_failed 1",
        "crawler_llm_config_invalid 1",
    ] {
        assert!(text.contains(name), "{name} missing in\n{text}");
    }

    // deploy.sh stopped reporting: the drift gauge is withdrawn (its absence alerts), the
    // certificate's expiry is still known. In sync again: drift 0.
    let later = now + CDuration::minutes(40);
    export_ops(&pool, &m, &OpsInputs::default(), later)
        .await
        .unwrap();
    assert_eq!(m.deploy_drift_seconds.get(), None);
    assert!(m.tls_cert_days_left.get().is_some());
    assert_eq!(m.llm_config_invalid.get(), Some(0));
    sqlx::query(
        "UPDATE heartbeats SET last_beat = $1, progress = progress || '{\"drift_since\": null}'
         WHERE role = 'host' AND instance = 'deploy'",
    )
    .bind(later)
    .execute(&pool)
    .await
    .unwrap();
    export_ops(&pool, &m, &OpsInputs::default(), later)
        .await
        .unwrap();
    assert_eq!(m.deploy_drift_seconds.get(), Some(0.0));
}
