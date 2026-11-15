//! Error types shared by every crate.

use crate::fetch::FetchError;
use crate::llm::LlmError;

/// The workspace-wide result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Coarse classification of an error, used for retry and back-off decisions and for metrics
/// (never per-URL labels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Worth retrying soon (network blip, 5xx, timeout).
    Transient,
    /// The remote asked us to slow down (429/503 with or without `Retry-After`).
    RateLimited,
    /// Retrying will not help (404/410, invalid input, schema violation).
    Permanent,
    /// Refused by our own policy (robots, denylist, SSRF, challenge, use policy).
    Policy,
    /// The claim's lease was lost or expired; stop work for this key immediately.
    Lease,
    /// The LLM budget or a spend breaker refused the call.
    Budget,
    /// A bug or an invariant violation.
    Internal,
}

/// Why a lease is no longer valid.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseLostReason {
    /// Database `now()` passed the claim deadline.
    Expired,
    /// A newer claim (higher epoch) owns the key.
    Superseded,
    /// The lease could not be verified (e.g. the database was unreachable). Callers fail closed.
    Unverifiable,
}

/// The lease behind a [`crate::frontier::ClaimPermit`] is gone: stop all requests for that key.
///
/// Any failure to *verify* a lease is also reported as `LeaseLost` (reason
/// [`LeaseLostReason::Unverifiable`]), so politeness fails closed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("lease lost for key {key} epoch {epoch}: {reason:?}")]
pub struct LeaseLost {
    /// The politeness key of the lost claim.
    pub key: String,
    /// The epoch of the lost claim.
    pub epoch: i64,
    /// Why it was lost.
    pub reason: LeaseLostReason,
}

/// The workspace-wide error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The claim's lease is gone.
    #[error(transparent)]
    LeaseLost(#[from] LeaseLost),
    /// A permit-gated fetch failed.
    #[error(transparent)]
    Fetch(#[from] FetchError),
    /// An LLM or embedding call failed or was refused.
    #[error(transparent)]
    Llm(#[from] LlmError),
    /// Input failed validation.
    #[error("invalid: {0}")]
    Invalid(String),
    /// The requested object does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// A write conflicted with existing state (e.g. a blob with the same path but other bytes).
    #[error("conflict: {0}")]
    Conflict(String),
    /// Refused by policy.
    #[error("policy: {0}")]
    Policy(String),
    /// A database error (message only; never includes row content).
    #[error("database: {0}")]
    Db(String),
    /// A blob-store or spool error.
    #[error("storage: {0}")]
    Storage(String),
    /// An I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Anything else, boxed.
    #[error(transparent)]
    Other(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl Error {
    /// Wraps any error as [`Error::Other`].
    pub fn other<E: std::error::Error + Send + Sync + 'static>(e: E) -> Self {
        Error::Other(Box::new(e))
    }

    /// The coarse class of this error.
    pub fn class(&self) -> ErrorClass {
        match self {
            Error::LeaseLost(_) => ErrorClass::Lease,
            Error::Fetch(e) => e.class(),
            Error::Llm(e) => e.class(),
            Error::Invalid(_) | Error::NotFound(_) => ErrorClass::Permanent,
            Error::Conflict(_) => ErrorClass::Internal,
            Error::Policy(_) => ErrorClass::Policy,
            Error::Db(_) | Error::Storage(_) | Error::Io(_) => ErrorClass::Transient,
            Error::Other(_) => ErrorClass::Internal,
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Invalid(e.to_string())
    }
}
