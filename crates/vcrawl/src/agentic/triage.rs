//! Link triage (spec §5.7): Gemini classifies in-scope links that the heuristics leave
//! uncertain as relevant or not, and which kind they would hold. Verdicts are cached **per URL
//! pattern** ([`url_pattern`]), so one answer covers every URL of that shape and a pattern is
//! never paid for twice. Links are sent in batches of [`BATCH`] patterns.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::Mutex;

use serde_json::{Value, json};
use vc_core::llm::{FinishReason, LlmRequest, LlmResponse, Purpose, ThinkingLevel, Usage};
use vc_core::{CanonicalUrl, ModelId, Result};
use vc_llm::{CacheKey, LlmCache};

use super::{AgenticLlm, Called, answer_json, clip};

/// The system prompt.
pub const PROMPT: &str = include_str!("../../../../prompts/agentic/triage.md");

/// Patterns per model call.
pub const BATCH: usize = 100;

/// Output cap for one batch (about 25 tokens per verdict).
const MAX_OUTPUT_TOKENS: u32 = 3000;

/// Model id under which pattern verdicts are kept in `llm_cache`.
const CACHE_MODEL: &str = "agentic-triage/v1";

/// One link the heuristics could not decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCandidate {
    /// The canonical link target.
    pub url: CanonicalUrl,
    /// Anchor text (clipped to 120 characters).
    pub anchor: String,
    /// Surrounding text (clipped to 200 characters).
    pub context: String,
}

/// The verdict for a pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageVerdict {
    /// Worth following.
    pub relevant: bool,
    /// The kind it would hold (one of the allowed kinds), when known.
    pub kind: Option<String>,
}

impl TriageVerdict {
    fn to_json(&self) -> Value {
        json!({"relevant": self.relevant, "kind": self.kind})
    }

    fn from_json(v: &Value, kinds: &[String]) -> Option<Self> {
        let relevant = v.get("relevant")?.as_bool()?;
        let kind = v
            .get("kind")
            .and_then(Value::as_str)
            .filter(|k| relevant && kinds.iter().any(|x| x == k))
            .map(str::to_string);
        Some(TriageVerdict { relevant, kind })
    }
}

/// The result of [`triage_links`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TriageReport {
    /// One entry per input link, same order; `None` = undecided (LLM off, budget, failure):
    /// the caller keeps its heuristic default.
    pub verdicts: Vec<Option<TriageVerdict>>,
    /// Distinct patterns among the input.
    pub patterns: usize,
    /// Patterns answered from the cache.
    pub cache_hits: usize,
    /// Model calls made.
    pub llm_calls: usize,
    /// Stopped early because the LLM is disabled or the budget refused.
    pub stopped: bool,
}

/// Where pattern verdicts are kept.
pub trait TriageCache: Send + Sync {
    /// The cached verdict for a pattern.
    fn get(&self, pattern: &str) -> impl Future<Output = Result<Option<Value>>> + Send;
    /// Stores a verdict (first writer wins).
    fn put(&self, pattern: &str, verdict: Value) -> impl Future<Output = Result<()>> + Send;
}

/// An in-process cache (tests, and a front cache for the processor).
#[derive(Debug, Default)]
pub struct MemTriageCache {
    map: Mutex<HashMap<String, Value>>,
}

impl MemTriageCache {
    /// Number of cached patterns.
    pub fn len(&self) -> usize {
        self.map.lock().expect("triage cache lock").len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl TriageCache for MemTriageCache {
    async fn get(&self, pattern: &str) -> Result<Option<Value>> {
        Ok(self
            .map
            .lock()
            .expect("triage cache lock")
            .get(pattern)
            .cloned())
    }

    async fn put(&self, pattern: &str, verdict: Value) -> Result<()> {
        self.map
            .lock()
            .expect("triage cache lock")
            .entry(pattern.to_string())
            .or_insert(verdict);
        Ok(())
    }
}

/// Verdicts in `llm_cache` (`model = agentic-triage/v1`, `prompt_hash` of the triage prompt,
/// `input_hash` of the pattern): a prompt change starts a fresh cache.
#[derive(Debug, Clone)]
pub struct PgTriageCache {
    cache: LlmCache,
}

impl PgTriageCache {
    /// A cache on `pool`.
    pub fn new(pool: sqlx::PgPool) -> Self {
        PgTriageCache {
            cache: LlmCache::new(pool),
        }
    }

    fn key(pattern: &str) -> CacheKey {
        CacheKey::of(
            CACHE_MODEL,
            &LlmRequest {
                model: None,
                purpose: Purpose::Triage,
                system: PROMPT.to_string(),
                input: pattern.to_string(),
                response_schema: Some(schema()),
                max_output_tokens: MAX_OUTPUT_TOKENS,
                thinking: ThinkingLevel::Off,
            },
        )
    }
}

impl TriageCache for PgTriageCache {
    async fn get(&self, pattern: &str) -> Result<Option<Value>> {
        Ok(self
            .cache
            .get(&Self::key(pattern))
            .await?
            .and_then(|r| r.json))
    }

    async fn put(&self, pattern: &str, verdict: Value) -> Result<()> {
        let resp = LlmResponse {
            text: verdict.to_string(),
            json: Some(verdict),
            usage: Usage::default(),
            model: ModelId::new(CACHE_MODEL),
            finish: FinishReason::Stop,
            from_cache: false,
        };
        self.cache.put(&Self::key(pattern), &resp).await
    }
}

/// The URL pattern a verdict covers: host, then path segments with numbers → `{n}`,
/// identifiers (digits mixed with letters, or 16+ hex chars) → `{id}`, title slugs (3+
/// hyphenated words) → `{slug}`; query parameter **names** only, sorted. Lowercased.
///
/// `https://www.ex.com/events/2026/11/jazz-night-at-the-park?page=2` →
/// `www.ex.com/events/{n}/{n}/{slug}?page`.
pub fn url_pattern(url: &CanonicalUrl) -> String {
    let u = url.as_url();
    let mut out = url.host().to_ascii_lowercase();
    for seg in u.path().split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        out.push_str(&segment_pattern(&seg.to_ascii_lowercase()));
    }
    if u.path().ends_with('/') && u.path() != "/" {
        out.push('/');
    }
    let names: BTreeSet<String> = u
        .query_pairs()
        .map(|(k, _)| k.to_ascii_lowercase())
        .collect();
    if !names.is_empty() {
        out.push('?');
        out.push_str(&names.into_iter().collect::<Vec<_>>().join("&"));
    }
    out
}

fn segment_pattern(seg: &str) -> String {
    let (stem, ext) = match seg.rsplit_once('.') {
        Some((s, e))
            if !s.is_empty() && e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphabetic()) =>
        {
            (s, Some(e))
        }
        _ => (seg, None),
    };
    let has_digit = stem.chars().any(|c| c.is_ascii_digit());
    let p = if !stem.is_empty() && stem.chars().all(|c| c.is_ascii_digit()) {
        "{n}".to_string()
    } else if stem.len() >= 16 && stem.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        "{id}".to_string()
    } else if stem.split('-').filter(|w| !w.is_empty()).count() >= 3 {
        "{slug}".to_string()
    } else if has_digit && stem.chars().any(|c| c.is_ascii_alphabetic()) {
        "{id}".to_string()
    } else {
        stem.to_string()
    };
    match ext {
        Some(e) => format!("{p}.{e}"),
        None => p,
    }
}

/// The response schema: `{items: [{i, relevant, kind}]}`.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "i": {"type": "integer"},
                        "relevant": {"type": "boolean"},
                        "kind": {"type": ["string", "null"]}
                    },
                    "required": ["i", "relevant"]
                }
            }
        },
        "required": ["items"]
    })
}

/// Triages `links`. Cached patterns are answered without a call; the rest go to Gemini in
/// batches of [`BATCH`] (one representative link per pattern) under purpose `triage`.
/// `kinds` are the record kinds a verdict may name (others are dropped).
pub async fn triage_links<C: TriageCache>(
    llm: &AgenticLlm,
    cache: &C,
    kinds: &[String],
    links: &[LinkCandidate],
) -> Result<TriageReport> {
    let patterns: Vec<String> = links.iter().map(|l| url_pattern(&l.url)).collect();
    // First link of each distinct pattern, in input order.
    let mut first: Vec<usize> = Vec::new();
    let mut seen: HashMap<&str, ()> = HashMap::new();
    for (i, p) in patterns.iter().enumerate() {
        if seen.insert(p.as_str(), ()).is_none() {
            first.push(i);
        }
    }
    let mut rep = TriageReport {
        patterns: first.len(),
        ..TriageReport::default()
    };
    let mut decided: HashMap<String, TriageVerdict> = HashMap::new();
    let mut todo: Vec<usize> = Vec::new();
    for &i in &first {
        match cache.get(&patterns[i]).await? {
            Some(v) => match TriageVerdict::from_json(&v, kinds) {
                Some(verdict) => {
                    rep.cache_hits += 1;
                    decided.insert(patterns[i].clone(), verdict);
                }
                None => todo.push(i),
            },
            None => todo.push(i),
        }
    }
    for chunk in todo.chunks(BATCH) {
        let items: Vec<Value> = chunk
            .iter()
            .enumerate()
            .map(|(n, &i)| {
                let l = &links[i];
                json!({
                    "i": n,
                    "pattern": patterns[i],
                    "url": l.url.as_str(),
                    "anchor": clip(&l.anchor, 120),
                    "context": clip(&l.context, 200),
                })
            })
            .collect();
        let input = json!({"kinds": kinds, "links": items}).to_string();
        let req = LlmRequest {
            model: None,
            purpose: Purpose::Triage,
            system: PROMPT.to_string(),
            input,
            response_schema: Some(schema()),
            max_output_tokens: MAX_OUTPUT_TOKENS,
            thinking: ThinkingLevel::Off,
        };
        let called = llm.call(req).await?;
        if called.stops() {
            rep.stopped = true;
            break;
        }
        rep.llm_calls += 1;
        let Called::Answer(resp) = called else {
            continue;
        };
        let Some(v) = answer_json(&resp) else {
            continue;
        };
        for item in v
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(n) = item.get("i").and_then(Value::as_u64) else {
                continue;
            };
            let Some(&i) = chunk.get(n as usize) else {
                continue;
            };
            let Some(verdict) = TriageVerdict::from_json(item, kinds) else {
                continue;
            };
            if decided.contains_key(&patterns[i]) {
                continue;
            }
            cache.put(&patterns[i], verdict.to_json()).await?;
            decided.insert(patterns[i].clone(), verdict);
        }
    }
    rep.verdicts = patterns.iter().map(|p| decided.get(p).cloned()).collect();
    Ok(rep)
}
