# Crate: vc-obs

**Purpose.** Observability for every role: JSON logs with a field allowlist, Prometheus
metrics, progress-based health and a heartbeat.

Path: `crates/vc-obs`. Dependencies: `tracing`, `tracing-subscriber`, `prometheus-client`,
`sqlx`. Status: wired. Every role logs. Every long-running role (scheduler, fetcher, processor, embedder, api, maintenance) also serves `/metrics` and `/healthz` on `obs.metrics_addr` and writes a heartbeat; `crates/vcrawl/tests/role_metrics.rs` checks each one.

## Modules

| Module | Key items | What it does |
|---|---|---|
| `logging` | `init_tracing`, `json_layer`, `pretty_layer`, `AllowlistLayer`, `ALLOWED_FIELDS`, `severity` | JSON lines for Cloud Logging (`timestamp`, `severity`, `role`, `target`, `message` + allowed fields). Any field not on the allowlist (`body`, `token`, `headers`, a library's `db.statement`, …) is dropped by name. Values are cut to 256 characters, messages to 512, so a page formatted into a message cannot reach the logs whole. |
| `metrics` | `Metrics`, `ALERT_METRICS`, `AlertMetric`, `cloud_monitoring_type` | the Prometheus registry; alert metrics carry the exact names the monitoring policies read |
| `health` | `ProgressTracker`, `Health` | unhealthy when there is due work but no progress for `progress_window_s` (600 s) |
| `heartbeat` | `HeartbeatWriter`, `heartbeat_freshness` | upserts a row in `heartbeats` per role |
| `ops` | `export_ops`, `OpsInputs`, `parse_openssl_date`, `DEPLOY_BEAT_STALE` | the alert gauges no role measures itself, set from Postgres every minute by the maintenance role (see below) |
| `server` | `serve`, `bind_metrics`, `probe`, `get` | `GET /metrics` and `GET /healthz` on `obs.metrics_addr` (default `0.0.0.0:9464`); `probe` backs `vcrawl health` |

## Alert metrics

Exposed as `crawler_<name>` and shipped as `custom.googleapis.com/crawler/<name>`:
`heartbeat` (label `role`), `captures_committed`, `due_urls`, `backpressure`,
`wal_archive_age_seconds`, `base_backup_age_seconds`, `restore_drill_ok`,
`restore_drill_age_seconds`, `llm_month_spend_ratio`, `parked_keys_ratio`,
`tls_cert_days_left`, `llm_config_invalid`, `deploy_drift_seconds`, `host_units_failed`,
`backup_restore_age_seconds`, `spool_quarantined` (counter).

`ops::export_ops` sets these from Postgres. A gauge whose source is missing or stale is
**unset**, not zero, so the policy's absence condition fires (an alert that checks only a
threshold can never fire on a gauge nobody exports).

| Gauge | Source |
|---|---|
| `wal_archive_age_seconds` | `pg_stat_archiver.last_archived_time` |
| `base_backup_age_seconds` | `heartbeats` row of the base-backup unit (`last_ok_at`, written by `deploy/host/unit-status.sh`) |
| `backup_restore_age_seconds` | the same for the light restore drill unit |
| `host_units_failed` | `heartbeats('host', 'unit:*')` rows with `failed = true` |
| `deploy_drift_seconds` | the `deploy` heartbeat's `drift_since` (from `deploy.sh`); unset when that row is older than 30 min |
| `tls_cert_days_left` | the same row's `tls_not_after` (the installed API certificate) |
| `llm_month_spend_ratio` | month spend ÷ monthly cap from `llm_budget` |
| `llm_config_invalid` | 1 when `llm.forced_off` is set (see [vc-config](Crate-vc-config)) |

## Config

`[obs]`: log format (`json` or `pretty`), log level, heartbeat interval (60 s), progress
window (600 s), metrics address.

## Tests

`tests/obs.rs`: `log_fields_allowlisted_no_body`, `health_fails_without_progress`,
`alert_metric_names_match_monitoring_policies` (reads `infrastructure/monitoring/*.json`),
`exported_when_known_gauges_alert_on_absence`, `metrics_values_and_heartbeat_absence`,
`heartbeat_writer_upserts_row`, `http_endpoint_serves_metrics_and_health`,
`ops_gauges_from_postgres`.
