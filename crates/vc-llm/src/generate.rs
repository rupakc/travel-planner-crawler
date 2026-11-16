//! `VertexLlm`: Gemini `generateContent` on Vertex AI (REST), plus the request/response shapes
//! shared with batch prediction.
//!
//! ```text
//! POST {base}/v1/projects/{p}/locations/global/publishers/google/models/{m}:generateContent
//! { "systemInstruction": {"parts": [{"text": …}]},
//!   "contents": [{"role": "user", "parts": [{"text": …}]}],
//!   "generationConfig": {"temperature": 0.1, "maxOutputTokens": 400,
//!                        "responseMimeType": "application/json", "responseSchema": {…},
//!                        "thinkingConfig": {"thinkingLevel": "MINIMAL"}},
//!   "labels": {"app": "voyager-crawler", "purpose": "extraction"} }
//! ```

use std::collections::BTreeMap;
use std::time::Instant;

use async_trait::async_trait;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::PgPool;
use vc_config::LlmConfig;
use vc_core::llm::{
    BudgetLedger, FinishReason, LlmClient, LlmError, LlmRequest, LlmResponse, Purpose, Reservation,
    ThinkingLevel, Usage,
};
use vc_core::{ModelId, Result};

use crate::cache::{CacheKey, LlmCache};
use crate::client::{VertexClient, labels};
use crate::pricing::Pricing;
use crate::schema::to_vertex_schema;

/// Sampling temperature for extraction-style calls.
pub const TEMPERATURE: f64 = 0.1;

/// The `thinkingConfig` for a level. Gemini 3 models take `thinkingLevel` (`MINIMAL` is the
/// lowest); older models take `thinkingBudget: 0` for off.
pub fn thinking_config(model: &str, level: ThinkingLevel) -> Value {
    match level {
        ThinkingLevel::Off if model.starts_with("gemini-2") => json!({"thinkingBudget": 0}),
        ThinkingLevel::Off | ThinkingLevel::Minimal => json!({"thinkingLevel": "MINIMAL"}),
        ThinkingLevel::Low => json!({"thinkingLevel": "LOW"}),
    }
}

/// The `generateContent` body (also one batch input line's `request`). No tools, ever.
pub fn generate_body(req: &LlmRequest, model: &str, labels: &BTreeMap<String, String>) -> Value {
    let mut gen_cfg = json!({
        "temperature": TEMPERATURE,
        "maxOutputTokens": req.max_output_tokens,
        "thinkingConfig": thinking_config(model, req.thinking),
    });
    if let Some(schema) = &req.response_schema {
        gen_cfg["responseMimeType"] = json!("application/json");
        gen_cfg["responseSchema"] = to_vertex_schema(schema);
    }
    let mut body = json!({
        "contents": [{"role": "user", "parts": [{"text": req.input}]}],
        "generationConfig": gen_cfg,
        "labels": labels,
    });
    if !req.system.is_empty() {
        body["systemInstruction"] = json!({"parts": [{"text": req.system}]});
    }
    body
}

/// Maps a Vertex `finishReason`.
pub fn finish_reason(s: &str) -> FinishReason {
    match s.trim_start_matches("FINISH_REASON_") {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::MaxTokens,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "MODEL_ARMOR" => {
            FinishReason::Safety
        }
        _ => FinishReason::Other,
    }
}

/// Usage from `usageMetadata` (`promptTokenCount`, `candidatesTokenCount`,
/// `thoughtsTokenCount`, `cachedContentTokenCount`), cost not set.
pub fn usage_of(v: &Value) -> Usage {
    let n = |k: &str| {
        v.pointer(&format!("/usageMetadata/{k}"))
            .and_then(Value::as_u64)
    };
    Usage {
        input_tokens: n("promptTokenCount").unwrap_or(0),
        output_tokens: n("candidatesTokenCount").unwrap_or(0),
        thinking_tokens: n("thoughtsTokenCount").unwrap_or(0),
        cached_input_tokens: n("cachedContentTokenCount").unwrap_or(0),
        cost: Default::default(),
    }
}

/// Parses a `GenerateContentResponse`. `None` when there is no candidate at all (blocked
/// prompt). Thought parts are skipped. `json` is set when a schema was requested and the text
/// parses.
pub fn parse_response(
    v: &Value,
    model: &ModelId,
    want_json: bool,
    pricing: &Pricing,
    batch: bool,
) -> LlmResponse {
    let mut usage = usage_of(v);
    usage.cost = pricing.cost(&usage, batch);
    let cand = v.pointer("/candidates/0");
    let text: String = cand
        .and_then(|c| c.pointer("/content/parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|p| !p.get("thought").and_then(Value::as_bool).unwrap_or(false))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let finish = match cand {
        None => FinishReason::Safety, // promptFeedback.blockReason
        Some(c) => c
            .get("finishReason")
            .and_then(Value::as_str)
            .map_or(FinishReason::Other, finish_reason),
    };
    let json = if want_json {
        serde_json::from_str::<Value>(text.trim()).ok()
    } else {
        None
    };
    LlmResponse {
        text,
        json,
        usage,
        model: model.clone(),
        finish,
        from_cache: false,
    }
}

/// What the startup probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The model answered.
    Available,
    /// `llm.enabled = false`: nothing was called.
    SkippedDisabled,
    /// The ledger refused the (tiny) reservation: nothing was called.
    SkippedBudget,
}

/// Gemini on Vertex AI (REST `generateContent`).
#[derive(Debug, Clone)]
pub struct VertexLlm {
    /// Settings.
    pub config: LlmConfig,
    vertex: VertexClient,
    pricing: Pricing,
    cache: Option<LlmCache>,
}

impl VertexLlm {
    /// A client without a response cache.
    pub fn new(config: LlmConfig, vertex: VertexClient) -> Self {
        let pricing = Pricing::from_config(&config);
        VertexLlm {
            config,
            vertex,
            pricing,
            cache: None,
        }
    }

    /// Adds the `llm_cache` on `pool`.
    pub fn with_cache(mut self, pool: PgPool) -> Self {
        self.cache = Some(LlmCache::new(pool));
        self
    }

    /// The shared Vertex client.
    pub fn vertex(&self) -> &VertexClient {
        &self.vertex
    }

    /// The reservation estimate for `req` (list price).
    pub fn estimate(&self, req: &LlmRequest) -> vc_core::llm::Usd {
        self.pricing.estimate(req, false)
    }

    fn model_of(&self, req: &LlmRequest) -> ModelId {
        req.model
            .clone()
            .unwrap_or_else(|| ModelId::new(&self.config.model))
    }

    /// The startup probe: a one-token call under a ledger reservation. A missing model fails
    /// loudly with [`LlmError::ModelUnavailable`].
    pub async fn probe(&self, ledger: &dyn BudgetLedger) -> Result<ProbeOutcome> {
        if !self.config.enabled {
            return Ok(ProbeOutcome::SkippedDisabled);
        }
        let req = LlmRequest {
            model: None,
            purpose: Purpose::Extraction,
            system: String::new(),
            input: "ping".into(),
            response_schema: None,
            max_output_tokens: 1,
            thinking: ThinkingLevel::Off,
        };
        let Some(r) = ledger.reserve(req.purpose, self.estimate(&req)).await? else {
            return Ok(ProbeOutcome::SkippedBudget);
        };
        let res = self.call(&req).await;
        let usage = res.as_ref().map(|x| x.usage.clone()).unwrap_or_default();
        ledger.settle(r, usage).await?;
        match res {
            // A 1-token answer may stop at MAX_TOKENS; that still proves the model exists.
            Ok(_) => Ok(ProbeOutcome::Available),
            Err(e) => Err(e.into()),
        }
    }

    async fn call(&self, req: &LlmRequest) -> std::result::Result<LlmResponse, LlmError> {
        let model = self.model_of(req);
        let body = generate_body(req, model.as_str(), &labels(req.purpose));
        let url = self
            .vertex
            .model_url(&self.config.location, model.as_str(), "generateContent");
        let started = Instant::now();
        let v = self
            .vertex
            .call(Method::POST, &url, &[], Some(&body))
            .await?;
        let resp = parse_response(
            &v,
            &model,
            req.response_schema.is_some(),
            &self.pricing,
            false,
        );
        tracing::info!(
            target: "llm_usage",
            purpose = req.purpose.as_str(),
            model = model.as_str(),
            input_tokens = resp.usage.input_tokens,
            output_tokens = resp.usage.output_tokens,
            thinking_tokens = resp.usage.thinking_tokens,
            cached_input_tokens = resp.usage.cached_input_tokens,
            cost_micro_usd = resp.usage.cost.micros(),
            latency_ms = started.elapsed().as_millis() as u64,
            "llm call"
        );
        Ok(resp)
    }
}

#[async_trait]
impl LlmClient for VertexLlm {
    async fn generate(&self, _r: &Reservation, req: LlmRequest) -> Result<LlmResponse> {
        if !self.config.enabled {
            return Err(LlmError::Disabled.into());
        }
        let key = CacheKey::of(self.model_of(&req).as_str(), &req);
        if let Some(c) = &self.cache
            && let Some(hit) = c.get(&key).await?
        {
            return Ok(hit);
        }
        let resp = self.call(&req).await?;
        let cacheable = resp.finish == FinishReason::Stop
            && (req.response_schema.is_none() || resp.json.is_some());
        if cacheable && let Some(c) = &self.cache {
            c.put(&key, &resp).await?;
        }
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reasons() {
        assert_eq!(finish_reason("STOP"), FinishReason::Stop);
        assert_eq!(finish_reason("FINISH_REASON_STOP"), FinishReason::Stop);
        assert_eq!(finish_reason("MAX_TOKENS"), FinishReason::MaxTokens);
        assert_eq!(finish_reason("PROHIBITED_CONTENT"), FinishReason::Safety);
        assert_eq!(finish_reason("MALFORMED_RESPONSE"), FinishReason::Other);
    }

    #[test]
    fn thinking_per_model() {
        assert_eq!(
            thinking_config("gemini-2.5-flash-lite", ThinkingLevel::Off),
            json!({"thinkingBudget": 0})
        );
        assert_eq!(
            thinking_config("gemini-3.1-flash-lite", ThinkingLevel::Off),
            json!({"thinkingLevel": "MINIMAL"})
        );
        assert_eq!(
            thinking_config("gemini-3.1-flash-lite", ThinkingLevel::Low),
            json!({"thinkingLevel": "LOW"})
        );
    }

    #[test]
    fn parse_skips_thoughts_and_blocked_prompt() {
        let p = Pricing::from_config(&LlmConfig::default());
        let m = ModelId::new("m");
        let v = json!({"candidates": [{"content": {"role": "model", "parts": [
            {"text": "hmm", "thought": true}, {"text": "{\"a\":1}"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5,
                              "thoughtsTokenCount": 3, "totalTokenCount": 18}});
        let r = parse_response(&v, &m, true, &p, false);
        assert_eq!(r.text, "{\"a\":1}");
        assert_eq!(r.json, Some(json!({"a": 1})));
        assert_eq!(r.usage.thinking_tokens, 3);
        let blocked = json!({"promptFeedback": {"blockReason": "SAFETY"}});
        let b = parse_response(&blocked, &m, true, &p, false);
        assert_eq!(b.finish, FinishReason::Safety);
        assert_eq!(b.json, None);
    }
}
