//! Prometheus metrics, scraped by the Ops Agent and forwarded to Cloud Monitoring.
//!
//! The metrics the alert policies in `infrastructure/monitoring/` read are listed in
//! [`ALERT_METRICS`]. Naming rule: Cloud Monitoring type `custom.googleapis.com/crawler/<x>` ⇔
//! Prometheus family `crawler_<x>` (counters get the standard `_total` suffix in the
//! exposition; [`cloud_monitoring_type`] strips it). A test checks the list against every
//! policy file and the infrastructure README.
//!
//! **Gauges are exported only once set.** Every role scrapes the same names, and a role that
//! does not own a value (the fetcher knows nothing about restore drills) must not export a 0
//! that would fire `restore_drill_ok < 1` or `tls_cert_days_left < 30`. The heartbeat series of
//! a role disappears when its last beat is older than the freshness window, so the
//! metric-absence alert ("crawler down") fires even when the process is alive but stuck.
//! Labels are low-cardinality only (role, kind, state): never per URL or per host.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use prometheus_client::encoding::prometheus_text;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::{Metric, Registry};
use vc_core::Clock;

/// The Prometheus type of an alert metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// Gauge (`GAUGE` in Cloud Monitoring).
    Gauge,
    /// Counter (`CUMULATIVE`).
    Counter,
}

/// A metric an alert policy reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlertMetric {
    /// Cloud Monitoring metric type (as in the policy filter).
    pub cloud_type: &'static str,
    /// Prometheus family name as exposed (counters end in `_total`).
    pub prometheus_name: &'static str,
    /// Prometheus type.
    pub kind: MetricKind,
    /// Labels.
    pub labels: &'static [&'static str],
    /// Who sets it.
    pub help: &'static str,
}

macro_rules! alert {
    ($x:literal, $prom:literal, $kind:ident, [$($l:literal),*], $help:literal) => {
        AlertMetric {
            cloud_type: concat!("custom.googleapis.com/crawler/", $x),
            prometheus_name: $prom,
            kind: MetricKind::$kind,
            labels: &[$($l),*],
            help: $help,
        }
    };
}

/// Every metric read by `infrastructure/monitoring/*.json` (README "Runtime contract").
pub const ALERT_METRICS: &[AlertMetric] = &[
    alert!(
        "heartbeat",
        "crawler_heartbeat",
        Gauge,
        ["role"],
        "1 while the role's heartbeat is fresh; absent otherwise (crawler-down alert)"
    ),
    alert!(
        "captures_committed",
        "crawler_captures_committed_total",
        Counter,
        [],
        "Captures committed by vc_commit_segment (no-captures alert)"
    ),
    alert!(
        "due_urls",
        "crawler_due_urls",
        Gauge,
        [],
        "URLs due for fetching now"
    ),
    alert!(
        "backpressure",
        "crawler_backpressure",
        Gauge,
        [],
        "1 while fetchers are paused by spool backpressure"
    ),
    alert!(
        "wal_archive_age_seconds",
        "crawler_wal_archive_age_seconds",
        Gauge,
        [],
        "Age of the newest archived WAL segment"
    ),
    alert!(
        "base_backup_age_seconds",
        "crawler_base_backup_age_seconds",
        Gauge,
        [],
        "Age of the newest base backup"
    ),
    alert!(
        "restore_drill_ok",
        "crawler_restore_drill_ok",
        Gauge,
        [],
        "1 when the last restore drill passed"
    ),
    alert!(
        "restore_drill_age_seconds",
        "crawler_restore_drill_age_seconds",
        Gauge,
        [],
        "Age of the last successful restore drill"
    ),
    alert!(
        "llm_month_spend_ratio",
        "crawler_llm_month_spend_ratio",
        Gauge,
        [],
        "LLM spend this month / monthly cap"
    ),
    alert!(
        "parked_keys_ratio",
        "crawler_parked_keys_ratio",
        Gauge,
        [],
        "Parked politeness keys / active keys"
    ),
    alert!(
        "tls_cert_days_left",
        "crawler_tls_cert_days_left",
        Gauge,
        [],
        "Days until the API TLS certificate expires"
    ),
];

/// Cloud Monitoring type for an exposed Prometheus family name: `crawler_<x>[_total]` →
/// `custom.googleapis.com/crawler/<x>`; `None` for names outside the `crawler_` namespace.
pub fn cloud_monitoring_type(prometheus_name: &str) -> Option<String> {
    let rest = prometheus_name.strip_prefix("crawler_")?;
    let rest = rest.strip_suffix("_total").unwrap_or(rest);
    if rest.is_empty() {
        return None;
    }
    Some(format!("custom.googleapis.com/crawler/{rest}"))
}

type Labels = Vec<(String, String)>;

/// An integer gauge that is exported only after its first [`IntGauge::set`].
#[derive(Debug, Clone, Default)]
pub struct IntGauge(Family<Labels, Gauge<i64, AtomicI64>>);

impl IntGauge {
    /// Sets the value (and starts exporting it).
    pub fn set(&self, v: i64) {
        self.0.get_or_create(&Vec::new()).set(v);
    }
    /// The value, if set.
    pub fn get(&self) -> Option<i64> {
        self.0.get(&Vec::new()).map(|g| g.get())
    }
    /// Stops exporting it.
    pub fn unset(&self) {
        self.0.clear();
    }
}

/// A float gauge that is exported only after its first [`FloatGauge::set`].
#[derive(Debug, Clone, Default)]
pub struct FloatGauge(Family<Labels, Gauge<f64, AtomicU64>>);

impl FloatGauge {
    /// Sets the value (and starts exporting it).
    pub fn set(&self, v: f64) {
        self.0.get_or_create(&Vec::new()).set(v);
    }
    /// The value, if set.
    pub fn get(&self) -> Option<f64> {
        self.0.get(&Vec::new()).map(|g| g.get())
    }
    /// Stops exporting it.
    pub fn unset(&self) {
        self.0.clear();
    }
}

/// The process's metrics: the alert metrics as typed handles, plus a registry other crates can
/// add their own (low-cardinality) metrics to.
#[derive(Debug)]
pub struct Metrics {
    clock: Arc<dyn Clock>,
    heartbeat_fresh: chrono::Duration,
    registry: RwLock<Registry>,
    heartbeat: Family<Labels, Gauge>,
    beats: Mutex<HashMap<String, DateTime<Utc>>>,
    /// `crawler_captures_committed_total`.
    pub captures_committed: Counter,
    /// `crawler_due_urls`.
    pub due_urls: IntGauge,
    /// `crawler_backpressure` (0/1).
    pub backpressure: IntGauge,
    /// `crawler_wal_archive_age_seconds`.
    pub wal_archive_age_seconds: FloatGauge,
    /// `crawler_base_backup_age_seconds`.
    pub base_backup_age_seconds: FloatGauge,
    /// `crawler_restore_drill_ok` (0/1).
    pub restore_drill_ok: IntGauge,
    /// `crawler_restore_drill_age_seconds`.
    pub restore_drill_age_seconds: FloatGauge,
    /// `crawler_llm_month_spend_ratio`.
    pub llm_month_spend_ratio: FloatGauge,
    /// `crawler_parked_keys_ratio`.
    pub parked_keys_ratio: FloatGauge,
    /// `crawler_tls_cert_days_left`.
    pub tls_cert_days_left: FloatGauge,
    /// `crawler_queue_items{kind,state}` (not alerted; for dashboards).
    pub queue_items: Family<Labels, Gauge>,
}

impl Metrics {
    /// A registry with every alert metric. A role's heartbeat series is exported while its last
    /// beat is at most `heartbeat_fresh` old (use about 3 × the heartbeat interval).
    pub fn new(clock: Arc<dyn Clock>, heartbeat_fresh: Duration) -> Self {
        let mut reg = Registry::default();
        let heartbeat = Family::<Labels, Gauge>::default();
        let captures_committed = Counter::default();
        let due_urls = IntGauge::default();
        let backpressure = IntGauge::default();
        let wal_archive_age_seconds = FloatGauge::default();
        let base_backup_age_seconds = FloatGauge::default();
        let restore_drill_ok = IntGauge::default();
        let restore_drill_age_seconds = FloatGauge::default();
        let llm_month_spend_ratio = FloatGauge::default();
        let parked_keys_ratio = FloatGauge::default();
        let tls_cert_days_left = FloatGauge::default();
        let queue_items = Family::<Labels, Gauge>::default();

        let help = |name: &str| {
            ALERT_METRICS
                .iter()
                .find(|m| m.prometheus_name.trim_end_matches("_total") == name)
                .map(|m| m.help)
                .unwrap_or("")
        };
        reg.register(
            "crawler_heartbeat",
            help("crawler_heartbeat"),
            heartbeat.clone(),
        );
        reg.register(
            "crawler_captures_committed",
            help("crawler_captures_committed"),
            captures_committed.clone(),
        );
        reg.register(
            "crawler_due_urls",
            help("crawler_due_urls"),
            due_urls.0.clone(),
        );
        reg.register(
            "crawler_backpressure",
            help("crawler_backpressure"),
            backpressure.0.clone(),
        );
        reg.register(
            "crawler_wal_archive_age_seconds",
            help("crawler_wal_archive_age_seconds"),
            wal_archive_age_seconds.0.clone(),
        );
        reg.register(
            "crawler_base_backup_age_seconds",
            help("crawler_base_backup_age_seconds"),
            base_backup_age_seconds.0.clone(),
        );
        reg.register(
            "crawler_restore_drill_ok",
            help("crawler_restore_drill_ok"),
            restore_drill_ok.0.clone(),
        );
        reg.register(
            "crawler_restore_drill_age_seconds",
            help("crawler_restore_drill_age_seconds"),
            restore_drill_age_seconds.0.clone(),
        );
        reg.register(
            "crawler_llm_month_spend_ratio",
            help("crawler_llm_month_spend_ratio"),
            llm_month_spend_ratio.0.clone(),
        );
        reg.register(
            "crawler_parked_keys_ratio",
            help("crawler_parked_keys_ratio"),
            parked_keys_ratio.0.clone(),
        );
        reg.register(
            "crawler_tls_cert_days_left",
            help("crawler_tls_cert_days_left"),
            tls_cert_days_left.0.clone(),
        );
        reg.register(
            "crawler_queue_items",
            "Work items per kind and state",
            queue_items.clone(),
        );

        Metrics {
            clock,
            heartbeat_fresh: chrono::Duration::from_std(heartbeat_fresh)
                .unwrap_or(chrono::Duration::MAX),
            registry: RwLock::new(reg),
            heartbeat,
            beats: Mutex::new(HashMap::new()),
            captures_committed,
            due_urls,
            backpressure,
            wal_archive_age_seconds,
            base_backup_age_seconds,
            restore_drill_ok,
            restore_drill_age_seconds,
            llm_month_spend_ratio,
            parked_keys_ratio,
            tls_cert_days_left,
            queue_items,
        }
    }

    /// Records a heartbeat of `role` now.
    pub fn heartbeat(&self, role: &str) {
        let now = self.clock.now();
        self.beats
            .lock()
            .expect("beats lock")
            .insert(role.to_string(), now);
    }

    /// Registers another metric (name should start with `crawler_`; low-cardinality labels).
    pub fn register(&self, name: &str, help: &str, metric: impl Metric) {
        self.registry
            .write()
            .expect("registry lock")
            .register(name, help, metric);
    }

    fn refresh_heartbeats(&self) {
        let now = self.clock.now();
        let beats = self.beats.lock().expect("beats lock");
        for (role, at) in beats.iter() {
            let labels = vec![("role".to_string(), role.clone())];
            if now - *at <= self.heartbeat_fresh {
                self.heartbeat.get_or_create(&labels).set(1);
            } else {
                self.heartbeat.remove(&labels);
            }
        }
    }

    /// The Prometheus text exposition (format 0.0.4, [`CONTENT_TYPE`]).
    pub fn encode(&self) -> String {
        self.refresh_heartbeats();
        let mut out = String::new();
        let reg = self.registry.read().expect("registry lock");
        // Writing into a String cannot fail.
        let _ = prometheus_text::encode(&mut out, &reg);
        out
    }
}

/// `Content-Type` of [`Metrics::encode`].
pub const CONTENT_TYPE: &str = prometheus_text::CONTENT_TYPE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_gauges_are_not_exported() {
        let m = Metrics::new(Arc::new(vc_core::SystemClock), Duration::from_secs(180));
        let text = m.encode();
        assert!(!text.contains("crawler_restore_drill_ok "), "{text}");
        assert!(!text.contains("crawler_tls_cert_days_left "), "{text}");
        assert!(text.contains("crawler_captures_committed_total 0"));
        m.restore_drill_ok.set(1);
        assert_eq!(m.restore_drill_ok.get(), Some(1));
        assert!(m.encode().contains("crawler_restore_drill_ok 1"));
        m.restore_drill_ok.unset();
        assert_eq!(m.restore_drill_ok.get(), None);
        assert!(!m.encode().contains("crawler_restore_drill_ok "));
    }

    #[test]
    fn extra_metrics_and_labels() {
        let m = Metrics::new(Arc::new(vc_core::SystemClock), Duration::from_secs(180));
        m.queue_items
            .get_or_create(&vec![
                ("kind".into(), "extract".into()),
                ("state".into(), "ready".into()),
            ])
            .set(5);
        let c = Counter::<u64>::default();
        m.register("crawler_fetches", "Fetches", c.clone());
        c.inc();
        let text = m.encode();
        assert!(text.contains("crawler_queue_items{kind=\"extract\",state=\"ready\"} 5"));
        assert!(text.contains("crawler_fetches_total 1"));
    }
}
