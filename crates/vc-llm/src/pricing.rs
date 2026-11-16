//! Cost estimates (for reservations) and actual cost (for settlement), in micro-USD.

use vc_config::LlmConfig;
use vc_core::llm::{LlmRequest, ThinkingLevel, Usage, Usd};

/// Thinking tokens assumed by the estimate for each level (the actual count settles).
pub fn thinking_allowance(level: ThinkingLevel) -> u64 {
    match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => 256,
        ThinkingLevel::Low => 2048,
    }
}

/// List prices from the `[llm]` config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pricing {
    /// USD per 1M input tokens.
    pub input_per_m: f64,
    /// USD per 1M output (and thinking) tokens.
    pub output_per_m: f64,
    /// USD per 1M cached input tokens.
    pub cached_per_m: f64,
    /// Batch price factor.
    pub batch_discount: f64,
    /// USD per 1,000 embedding characters.
    pub embed_per_1k_chars: f64,
    /// Conservative chars-per-token ratio.
    pub chars_per_token: f64,
}

impl Pricing {
    /// From the config.
    pub fn from_config(c: &LlmConfig) -> Self {
        Pricing {
            input_per_m: c.input_usd_per_m,
            output_per_m: c.output_usd_per_m,
            cached_per_m: c.cached_input_usd_per_m,
            batch_discount: c.batch_discount,
            embed_per_1k_chars: c.embedding_usd_per_1k_chars,
            chars_per_token: if c.chars_per_token > 0.0 {
                c.chars_per_token
            } else {
                3.5
            },
        }
    }

    /// Tokens for `chars` characters at the conservative ratio (rounded up).
    pub fn tokens_for_chars(&self, chars: usize) -> u64 {
        (chars as f64 / self.chars_per_token).ceil() as u64
    }

    /// The reservation estimate: input tokens (system + input + schema, chars/3.5) at input
    /// price, plus `maxOutputTokens` and the thinking allowance at output price.
    pub fn estimate(&self, req: &LlmRequest, batch: bool) -> Usd {
        let schema_chars = req
            .response_schema
            .as_ref()
            .map_or(0, |s| s.to_string().chars().count());
        let chars = req.system.chars().count() + req.input.chars().count() + schema_chars;
        let input = self.tokens_for_chars(chars);
        let output = u64::from(req.max_output_tokens) + thinking_allowance(req.thinking);
        self.price(input, 0, output, batch)
    }

    /// The cost of actual usage (thinking billed as output).
    pub fn cost(&self, u: &Usage, batch: bool) -> Usd {
        let cached = u.cached_input_tokens.min(u.input_tokens);
        self.price(
            u.input_tokens - cached,
            cached,
            u.output_tokens + u.thinking_tokens,
            batch,
        )
    }

    fn price(&self, input: u64, cached: u64, output: u64, batch: bool) -> Usd {
        // micro-USD per token == USD per 1M tokens.
        let f = if batch { self.batch_discount } else { 1.0 };
        let micros = (input as f64 * self.input_per_m
            + cached as f64 * self.cached_per_m
            + output as f64 * self.output_per_m)
            * f;
        Usd::from_micros(micros.ceil() as i64)
    }

    /// The cost of embedding texts with `chars` characters in total.
    pub fn embed_cost(&self, chars: usize) -> Usd {
        // USD per 1k chars × 1e6 / 1e3 = micro-USD per char × 1000.
        Usd::from_micros((chars as f64 * self.embed_per_1k_chars * 1000.0).ceil() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vc_core::llm::Purpose;

    fn req(input: &str) -> LlmRequest {
        LlmRequest {
            model: None,
            purpose: Purpose::Extraction,
            system: String::new(),
            input: input.into(),
            response_schema: None,
            max_output_tokens: 400,
            thinking: ThinkingLevel::Off,
        }
    }

    #[test]
    fn estimate_counts_chars_and_max_output() {
        let p = Pricing::from_config(&LlmConfig::default());
        // 7 chars / 3.5 = 2 tokens × 0.25 + 400 × 1.50 = 600.5 → 601 micro-USD.
        assert_eq!(p.estimate(&req("abcdefg"), false), Usd(601));
        assert_eq!(p.estimate(&req("abcdefg"), true), Usd(301));
    }

    #[test]
    fn cost_bills_thinking_as_output_and_cached_cheaper() {
        let p = Pricing::from_config(&LlmConfig::default());
        let u = Usage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            thinking_tokens: 100_000,
            cached_input_tokens: 0,
            cost: Usd::ZERO,
        };
        assert_eq!(p.cost(&u, false), Usd::from_dollars(0.25 + 0.30));
        let c = Usage {
            input_tokens: 1_000_000,
            cached_input_tokens: 1_000_000,
            ..Usage::default()
        };
        assert_eq!(p.cost(&c, false), Usd::from_dollars(0.025));
    }

    #[test]
    fn embed_cost_per_chars() {
        let p = Pricing::from_config(&LlmConfig::default());
        assert_eq!(p.embed_cost(1_000_000), Usd::from_dollars(0.025));
    }
}
