//! A scripted fake LLM (also a deterministic `Embedder`) and an in-memory budget ledger.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Datelike;
use vc_core::llm::{
    BudgetLedger, Embedder, FinishReason, LlmClient, LlmError, LlmRequest, LlmResponse, Purpose,
    Reservation, Usage, Usd,
};
use vc_core::{Clock, Error, ModelId, ReservationId, Result};

/// A scripted LLM: responses are popped in order; every request is recorded. When disabled,
/// every call returns [`LlmError::Disabled`] (like `VC_LLM_ENABLED=false`).
#[derive(Debug)]
pub struct FakeLlm {
    model: ModelId,
    dim: usize,
    enabled: AtomicBool,
    script: Mutex<VecDeque<std::result::Result<LlmResponse, LlmError>>>,
    requests: Mutex<Vec<LlmRequest>>,
    embed_calls: Mutex<Vec<Vec<String>>>,
}

impl Default for FakeLlm {
    fn default() -> Self {
        FakeLlm::new()
    }
}

impl FakeLlm {
    /// An enabled fake with model `fake-model` and embedding dimension 768.
    pub fn new() -> Self {
        FakeLlm {
            model: ModelId::new("fake-model"),
            dim: 768,
            enabled: AtomicBool::new(true),
            script: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            embed_calls: Mutex::new(Vec::new()),
        }
    }

    /// Sets the embedding dimension.
    pub fn with_dim(mut self, dim: usize) -> Self {
        self.dim = dim;
        self
    }

    /// Enables or disables the fake.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
    }

    /// A response whose text is `v` serialised and whose `json` is `v`.
    pub fn json_response(&self, v: serde_json::Value, usage: Usage) -> LlmResponse {
        LlmResponse {
            text: v.to_string(),
            json: Some(v),
            usage,
            model: self.model.clone(),
            finish: FinishReason::Stop,
            from_cache: false,
        }
    }

    /// Scripts a JSON response (zero usage).
    pub fn push_json(&self, v: serde_json::Value) {
        let r = self.json_response(v, Usage::default());
        self.push(Ok(r));
    }

    /// Scripts any outcome.
    pub fn push(&self, r: std::result::Result<LlmResponse, LlmError>) {
        self.script.lock().expect("fake llm lock").push_back(r);
    }

    /// Requests received so far.
    pub fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().expect("fake llm lock").clone()
    }

    /// Number of `generate` calls that reached the fake while enabled.
    pub fn calls(&self) -> usize {
        self.requests.lock().expect("fake llm lock").len()
    }

    /// Number of `embed` calls.
    pub fn embed_calls(&self) -> usize {
        self.embed_calls.lock().expect("fake llm lock").len()
    }

    /// The deterministic unit vector the fake returns for `text`.
    pub fn vector_for(&self, text: &str) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.dim);
        let mut h = blake3::Hasher::new();
        h.update(text.as_bytes());
        let mut rd = h.finalize_xof();
        let mut buf = [0u8; 4];
        for _ in 0..self.dim {
            rd.fill(&mut buf);
            out.push((u32::from_le_bytes(buf) as f32 / u32::MAX as f32) * 2.0 - 1.0);
        }
        let norm = out
            .iter()
            .map(|x| x * x)
            .sum::<f32>()
            .sqrt()
            .max(f32::EPSILON);
        out.iter_mut().for_each(|x| *x /= norm);
        out
    }
}

#[async_trait]
impl LlmClient for FakeLlm {
    async fn generate(&self, _r: &Reservation, req: LlmRequest) -> Result<LlmResponse> {
        if !self.enabled.load(Ordering::SeqCst) {
            return Err(LlmError::Disabled.into());
        }
        self.requests.lock().expect("fake llm lock").push(req);
        match self.script.lock().expect("fake llm lock").pop_front() {
            Some(Ok(r)) => Ok(r),
            Some(Err(e)) => Err(e.into()),
            None => Err(Error::Invalid("FakeLlm: no scripted response left".into())),
        }
    }
}

#[async_trait]
impl Embedder for FakeLlm {
    fn model(&self) -> &ModelId {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed(&self, _r: &Reservation, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if !self.enabled.load(Ordering::SeqCst) {
            return Err(LlmError::Disabled.into());
        }
        self.embed_calls
            .lock()
            .expect("fake llm lock")
            .push(texts.to_vec());
        Ok(texts.iter().map(|t| self.vector_for(t)).collect())
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Totals {
    reserved: Usd,
    spent: Usd,
}

#[derive(Debug, Default)]
struct LedgerInner {
    periods: HashMap<String, Totals>,
    open: HashMap<ReservationId, Reservation>,
}

/// An in-memory ledger with a day cap and a month cap, enforced atomically under one lock
/// (purpose shares and the dynamic daily cap are `PgLedger` concerns).
#[derive(Debug)]
pub struct MemLedger {
    clock: Arc<dyn Clock>,
    day_cap: Usd,
    month_cap: Usd,
    inner: Mutex<LedgerInner>,
}

impl MemLedger {
    /// A ledger with the given caps.
    pub fn new(clock: Arc<dyn Clock>, day_cap: Usd, month_cap: Usd) -> Self {
        MemLedger {
            clock,
            day_cap,
            month_cap,
            inner: Mutex::new(LedgerInner::default()),
        }
    }

    fn periods(&self) -> (chrono::NaiveDate, String, String) {
        let d = self.clock.now().date_naive();
        (
            d,
            d.format("%Y-%m-%d").to_string(),
            format!("{:04}-{:02}", d.year(), d.month()),
        )
    }

    /// `(reserved, spent)` for a period key (`YYYY-MM-DD` or `YYYY-MM`).
    pub fn totals(&self, period: &str) -> (Usd, Usd) {
        let g = self.inner.lock().expect("ledger lock");
        let t = g.periods.get(period).copied().unwrap_or_default();
        (t.reserved, t.spent)
    }

    /// Open reservations.
    pub fn open_reservations(&self) -> usize {
        self.inner.lock().expect("ledger lock").open.len()
    }

    fn reserve_inner(&self, purpose: Purpose, est: Usd, batch: bool) -> Option<Reservation> {
        let (day, dkey, mkey) = self.periods();
        let mut g = self.inner.lock().expect("ledger lock");
        let d = g.periods.get(&dkey).copied().unwrap_or_default();
        let m = g.periods.get(&mkey).copied().unwrap_or_default();
        let fits =
            |t: Totals, cap: Usd| t.reserved.saturating_add(t.spent).saturating_add(est) <= cap;
        if est < Usd::ZERO || !fits(d, self.day_cap) || !fits(m, self.month_cap) {
            return None;
        }
        for k in [&dkey, &mkey] {
            let t = g.periods.entry(k.clone()).or_default();
            t.reserved = t.reserved.saturating_add(est);
        }
        let r = Reservation {
            id: ReservationId::new(),
            purpose,
            est,
            day,
            month: mkey,
            batch,
            created_at: self.clock.now(),
        };
        g.open.insert(r.id, r.clone());
        Some(r)
    }
}

#[async_trait]
impl BudgetLedger for MemLedger {
    async fn reserve(&self, purpose: Purpose, est: Usd) -> Result<Option<Reservation>> {
        Ok(self.reserve_inner(purpose, est, false))
    }

    async fn reserve_batch(&self, purpose: Purpose, est: Usd) -> Result<Option<Reservation>> {
        Ok(self.reserve_inner(purpose, est, true))
    }

    async fn settle(&self, r: Reservation, actual: Usage) -> Result<()> {
        let mut g = self.inner.lock().expect("ledger lock");
        if g.open.remove(&r.id).is_none() {
            return Ok(()); // idempotent
        }
        let dkey = r.day.format("%Y-%m-%d").to_string();
        for k in [dkey, r.month.clone()] {
            let t = g.periods.entry(k).or_default();
            t.reserved = t.reserved.saturating_sub(r.est);
            t.spent = t.spent.saturating_add(actual.cost);
        }
        Ok(())
    }
}
