//! # vc-core
//!
//! Shared ids, value types and **every cross-crate trait** of the Voyager crawler.
//!
//! The P1 crates depend only on `vc-core`, `vc-config` and (as a dev-dependency) `testkit`;
//! only the `vcrawl` binary depends on implementations (a star graph, r5 Q1). This crate is
//! **frozen after P0**: changes go through the controller as additive, batched PRs.
//!
//! Module map (one module per seam):
//!
//! | Module | Seam | Implemented by |
//! |---|---|---|
//! | [`ids`], [`url`], [`media`] | identities, canonical URLs, media types | here |
//! | [`usepolicy`], [`licence`] | opt-out signals → allowed uses; licence/source classes | here (matrix), `vc-policy` |
//! | [`policy`] | robots, denylist, SSRF, challenge detection (pure) | `vc-policy` |
//! | [`frontier`] | claims, fenced leases, rate tokens, enqueue, park | `vc-frontier` |
//! | [`fetch`] | permit-gated HTTP | `vc-fetch` |
//! | [`render`] | Chromium rendering through CDP interception | `vc-render` |
//! | [`store`] | blobs, spool, segment commit | `vc-store` |
//! | [`parse`] | content handlers per media type | `vc-parse` |
//! | [`kinds`] | data-driven kind registry | `vc-kinds` |
//! | [`llm`] | LLM, embeddings, budget ledger, batch runner | `vc-llm` |
//! | [`extract`], [`records`] | place resolution; extracted records + provenance | `vc-extract` |
//! | [`sources`] | `SourceSpec` and API/dump adapters | `vc-sources` |
//! | [`queue`] | work queue | `vc-queue` |
//! | [`index`] | record sink and read model | `vc-index` |
//! | [`secrets`] | secret lookup | `vc-secrets` |
//!
//! **No bot-detection evasion.** There is exactly one identity, [`USER_AGENT`]; a challenge, 401
//! or 403 means *detect → park*, never retry with another identity.

pub mod clock;
pub mod error;
pub mod extract;
pub mod fetch;
pub mod frontier;
pub mod ids;
pub mod index;
pub mod kinds;
pub mod licence;
pub mod llm;
pub mod media;
pub mod parse;
pub mod policy;
pub mod queue;
pub mod records;
pub mod render;
pub mod secrets;
pub mod sources;
pub mod store;
pub mod url;
pub mod usepolicy;

pub use crate::clock::{Clock, SystemClock};
pub use crate::error::{Error, ErrorClass, LeaseLost, LeaseLostReason, Result};
pub use crate::ids::{
    BatchJobId, CaptureId, CaptureRef, DbTime, EntityId, Epoch, ExtractionRunId, KindId, ModelId,
    OriginKey, PayloadHash, PolitenessKey, RecordId, RecordKey, ReservationId, SegmentId, SourceId,
    UrlKey, WorkId,
};
pub use crate::url::{CanonicalUrl, UrlError};

/// The robots.txt product token we obey (RFC 9309 §2.2.1). Groups for this token win over `*`.
pub const ROBOTS_TOKEN: &str = "VoyagerBot";

/// The URL of the page that explains the bot, how to opt out, and the published egress IP.
pub const BOT_INFO_URL: &str = "https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md";

/// The one and only `User-Agent`, used verbatim for HTTP and for Chromium.
///
/// `VoyagerBot/<crate version> (+<BOT.md URL>)`. Never rotate, randomise or spoof it.
pub const USER_AGENT: &str = concat!(
    "VoyagerBot/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md)"
);

/// Maximum number of URLs a single claim may lease (spec §17.2).
pub const MAX_URLS_PER_CLAIM: usize = 5;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_is_the_documented_constant() {
        assert!(USER_AGENT.starts_with("VoyagerBot/"));
        assert!(USER_AGENT.ends_with(&format!("(+{BOT_INFO_URL})")));
        assert!(USER_AGENT.contains(ROBOTS_TOKEN));
    }
}
