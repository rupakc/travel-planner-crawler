//! JSON logs for Cloud Logging with a **field allowlist** (spec §9, hard rule 10).
//!
//! Only the fields in [`ALLOWED_FIELDS`] are written, from the event and from its enclosing
//! spans; every other field (`body`, `token`, `headers`, a library's `db.statement`, …) is
//! dropped by name, whatever its value. String values are cut to [`MAX_VALUE_CHARS`] and the
//! message to [`MAX_MESSAGE_CHARS`], so a page formatted into a message can't reach the logs
//! whole. Each line carries `timestamp`, `severity` (Cloud Logging names), `role`, `target`
//! and `message`.

use std::fmt::Write as _;
use std::io::Write as _;

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Field names that may appear in a log line. Ids, coarse classes and counters only: never page
/// content, URLs with query strings, headers, tokens or secrets. There is deliberately no
/// `error` field: error messages can quote input (a schema violation quotes the extracted
/// value), so log `error_class` (`vc_core::ErrorClass`) and a fixed `reason` instead.
pub const ALLOWED_FIELDS: &[&str] = &[
    // spec §9
    "role",
    "key",
    "url_key",
    "source_id",
    "capture_id",
    "epoch",
    // operational ids and classes
    "instance",
    "work_id",
    "kind",
    "state",
    "segment_id",
    "record_id",
    "batch_id",
    "purpose",
    "model",
    "media_type",
    "fetch_mode",
    "status",
    "error_class",
    "reason",
    "attempts",
    "count",
    "bytes",
    "duration_ms",
    "cost_usd",
    "phase",
    "version",
];

/// Longest message kept, in characters (longer ones are cut and end with `…`).
pub const MAX_MESSAGE_CHARS: usize = 512;

/// Longest string field value kept, in characters.
pub const MAX_VALUE_CHARS: usize = 256;

fn allowed(name: &str) -> bool {
    ALLOWED_FIELDS.contains(&name)
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Collects the allowlisted fields of an event or span.
#[derive(Default)]
struct Fields {
    map: Map<String, Value>,
    message: Option<String>,
}

impl Fields {
    fn put(&mut self, field: &Field, v: Value) {
        if allowed(field.name()) {
            self.map.insert(field.name().to_string(), v);
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(cut(value, MAX_MESSAGE_CHARS));
        } else {
            self.put(field, Value::String(cut(value, MAX_VALUE_CHARS)));
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::from(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::from(value));
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        if allowed(field.name()) {
            self.put(field, Value::String(cut(&value.to_string(), MAX_VALUE_CHARS)));
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let mut s = String::new();
            let _ = write!(s, "{value:?}");
            self.message = Some(cut(&s, MAX_MESSAGE_CHARS));
        } else if allowed(field.name()) {
            let mut s = String::new();
            let _ = write!(s, "{value:?}");
            self.put(field, Value::String(cut(&s, MAX_VALUE_CHARS)));
        }
    }
}

/// The allowlisted fields of a span, kept in its extensions.
struct SpanFields(Map<String, Value>);

/// Output format of [`AllowlistLayer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// One JSON object per line (Cloud Logging).
    Json,
    /// `timestamp SEVERITY role target: message k=v …` (development).
    Pretty,
}

/// A `tracing` layer that writes allowlisted fields only.
#[derive(Debug)]
pub struct AllowlistLayer<W> {
    role: String,
    format: Format,
    writer: W,
}

/// A JSON [`AllowlistLayer`] for `role`, writing to `writer`.
pub fn json_layer<W>(role: &str, writer: W) -> AllowlistLayer<W>
where
    W: for<'a> MakeWriter<'a> + 'static,
{
    AllowlistLayer {
        role: role.to_string(),
        format: Format::Json,
        writer,
    }
}

/// A human-readable [`AllowlistLayer`] (same allowlist) for development.
pub fn pretty_layer<W>(role: &str, writer: W) -> AllowlistLayer<W>
where
    W: for<'a> MakeWriter<'a> + 'static,
{
    AllowlistLayer {
        role: role.to_string(),
        format: Format::Pretty,
        writer,
    }
}

/// Cloud Logging severity of a level.
pub fn severity(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARNING",
        Level::INFO => "INFO",
        _ => "DEBUG",
    }
}

impl<S, W> Layer<S> for AllowlistLayer<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + 'static,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut f = Fields::default();
        attrs.record(&mut f);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields(f.map));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let mut f = Fields::default();
        values.record(&mut f);
        if let Some(span) = ctx.span(id) {
            let mut ext = span.extensions_mut();
            match ext.get_mut::<SpanFields>() {
                Some(existing) => existing.0.extend(f.map),
                None => ext.insert(SpanFields(f.map)),
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Map::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(sf) = span.extensions().get::<SpanFields>() {
                    fields.extend(sf.0.clone());
                }
            }
        }
        let mut f = Fields::default();
        event.record(&mut f);
        fields.extend(f.map);
        let meta = event.metadata();
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let message = f.message.unwrap_or_default();
        let line = match self.format {
            Format::Json => {
                let mut obj = Map::new();
                obj.insert("timestamp".into(), Value::String(ts));
                obj.insert("severity".into(), severity(meta.level()).into());
                obj.insert("role".into(), Value::String(self.role.clone()));
                obj.insert("target".into(), meta.target().into());
                obj.insert("message".into(), Value::String(message));
                for (k, v) in fields {
                    // `role` is the process role; a field cannot override it.
                    if k != "role" {
                        obj.insert(k, v);
                    }
                }
                let mut s = Value::Object(obj).to_string();
                s.push('\n');
                s
            }
            Format::Pretty => {
                let mut s = format!(
                    "{ts} {:<7} {} {}: {message}",
                    severity(meta.level()),
                    self.role,
                    meta.target()
                );
                for (k, v) in fields {
                    if k != "role" {
                        let _ = write!(s, " {k}={v}");
                    }
                }
                s.push('\n');
                s
            }
        };
        let mut w = self.writer.make_writer_for(meta);
        let _ = w.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_keeps_short_and_marks_long() {
        assert_eq!(cut("abc", 3), "abc");
        assert_eq!(cut("abcd", 3), "abc…");
        assert_eq!(cut("ééé", 2), "éé…");
    }

    #[test]
    fn severities() {
        assert_eq!(severity(&Level::WARN), "WARNING");
        assert_eq!(severity(&Level::TRACE), "DEBUG");
    }

    #[test]
    fn content_like_names_are_not_allowed() {
        for n in [
            "body",
            "content",
            "html",
            "text",
            "token",
            "authorization",
            "headers",
            "password",
            "secret",
            "url",
            "prompt",
            "payload",
            "error",
        ] {
            assert!(!allowed(n), "{n}");
        }
    }
}
