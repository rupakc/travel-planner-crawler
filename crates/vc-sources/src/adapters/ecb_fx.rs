//! `ecb_fx`: the ECB euro foreign exchange reference rates (`eurofxref-daily.xml`, also the
//! 90-day and historical XML variants, which share the format). One request per run.

use async_trait::async_trait;
use quick_xml::events::Event;
use serde::Serialize;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{Error, Result};

use super::common::{Fetch, check_status, fetch_raw, xml_attr};

/// One reference rate: 1 EUR = `rate` `currency` on `date`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FxRate {
    /// The reference date (`YYYY-MM-DD`).
    pub date: String,
    /// ISO 4217 code.
    pub currency: String,
    /// Units of `currency` per euro.
    pub rate: f64,
}

/// Parses the `gesmes:Envelope` → `Cube time` → `Cube currency rate` structure.
pub fn parse(body: &[u8]) -> Result<Vec<FxRate>> {
    let text = std::str::from_utf8(body).map_err(|e| Error::Invalid(e.to_string()))?;
    let mut r = quick_xml::Reader::from_str(text);
    let mut date: Option<String> = None;
    let mut out = Vec::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.local_name().as_ref() == "Cube" => {
                if let Some(t) = xml_attr(&e, "time") {
                    date = Some(t);
                }
                if let (Some(c), Some(v), Some(d)) =
                    (xml_attr(&e, "currency"), xml_attr(&e, "rate"), &date)
                {
                    let rate: f64 = v
                        .trim()
                        .parse()
                        .map_err(|_| Error::Invalid(format!("ecb: bad rate for {c}")))?;
                    if !(rate.is_finite() && rate > 0.0) {
                        return Err(Error::Invalid(format!("ecb: bad rate for {c}")));
                    }
                    out.push(FxRate {
                        date: d.clone(),
                        currency: c,
                        rate,
                    });
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(Error::Invalid(format!("ecb xml: {e}"))),
            _ => {}
        }
    }
    if out.is_empty() {
        return Err(Error::Invalid("ecb: no rates".into()));
    }
    Ok(out)
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct EcbFx;

#[async_trait]
impl SourceAdapter for EcbFx {
    fn id(&self) -> &'static str {
        "ecb_fx"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let mut rep = AdapterReport::default();
        let url = ctx.url.url.clone();
        let opts = Fetch {
            accept: Some("application/xml, text/xml"),
            hint: Some(MediaType::Xml),
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, url, &opts, &mut rep).await?;
        check_status(&ex)?;
        let rates = parse(&ex.body)?;
        tracing::info!(adapter = "ecb_fx", rates = rates.len(), "parsed");
        rep.complete = true;
        Ok(rep)
    }
}
