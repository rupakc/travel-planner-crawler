# Crate: vc-obs

**Purpose.** Observability for every role: JSON logs with a field allowlist, Prometheus
metrics, progress-based health and a heartbeat.

Path: `crates/vc-obs`. Dependencies: `tracing`, `tracing-subscriber`, `prometheus-client`,
`sqlx`.

## Modules

| Module | Key items | What it does |
|---|---|---|
| `logging` | `init_tracing`, `json_layer`, `pretty_layer`, `AllowlistLayer`, `ALLOWED_FIELDS`, `severity` | JSON lines for Cloud Logging (`timestamp`, `severity`, `role`, `target`, `message` + allowed fields). Any field not on the allowlist (`body`, `token`, `headers`, a library's `db.statement`, …) is dropped by name. Values are cut to 256 characters, messages to 512, so a page formatted into a message cannot reach the logs whole. |
| `metrics` | `Metrics`, `ALERT_METRICS`, `AlertMetric`, `cloud_monitoring_type` | the Prometheus registry; alert metrics carry the exact names the monitoring policies read |
| `health` | `ProgressTracker`, `Health` | unhealthy when there is due work but no progress for `progress_window_s` (600 s) |
| `heartbeat` | `HeartbeatWriter`, `heartbeat_freshness` | upserts a row in `heartbeats` per role |
| `server` | `serve`, `bind_metrics`, `probe`, `get` | `GET /metrics` and `GET /healthz` on `obs.metrics_addr` (default `0.0.0.0:9464`); `probe` backs `vcrawl health` |

## Alert metrics

Exposed as `crawler_<name>` and shipped as `custom.googleapis.com/crawler/<name>`:
`heartbeat` (label `role`), `captures_committed`, `due_urls`, `backpressure`,
`wal_archive_age_seconds`, `base_backup_age_seconds`, `restore_drill_ok`,
`restore_drill_age_seconds`, `llm_month_spend_ratio`, `parked_keys_ratio`,
`tls_cert_days_left`.

## Config

`[obs]`: log format (`json` or `pretty`), log level, heartbeat interval (60 s), progress
window (600 s), metrics address.

## Tests

`tests/obs.rs`: `log_fields_allowlisted_no_body`, `health_fails_without_progress`,
`alert_metric_names_match_monitoring_policies` (reads `infrastructure/monitoring/*.json`),
`metrics_values_and_heartbeat_absence`, `heartbeat_writer_upserts_row`,
`http_endpoint_serves_metrics_and_health`.
