//! Opt-out signals and the uses they allow (spec §5.2 UsePolicy matrix, r5 Q3).
//!
//! [`UsePolicy`] is a **meet-semilattice**: every dimension is a total order and
//! [`UsePolicy::meet`] takes the minimum per dimension. Combining signals can therefore only
//! restrict (fail closed). The matrix lives here, in [`UsePolicy::from_signals`], so every crate
//! reads the same table; `vc-policy::use_policy` delegates to it.
//!
//! | Signal | raw | index/serve | LLM | embed | serve to agents |
//! |---|---|---|---|---|---|
//! | none | archive | full | ✓ | ✓ | full |
//! | `noindex` (meta or `X-Robots-Tag`) | archive | none | ✓ | ✗ | facts only |
//! | `noarchive` | ephemeral (change detection, purged after 30 d) | full | ✓ | ✓ | full (no cached copy) |
//! | TDM reservation · `Content-Signal: ai-input=no` · AI-bot block in robots | archive | title + link | ✗ | ✗ | link only |
//! | `ai-train=no` | recorded only (we never train) | | | | |

use serde::{Deserialize, Serialize};

/// The opt-out signals observed for a capture. Robots and header signals are filled by the
/// fetcher; body signals (meta robots, TDMRep `<link>`) by the processor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UseSignals {
    /// `noindex` from meta robots or `X-Robots-Tag` (for our token or all robots).
    pub noindex: bool,
    /// `noarchive` from meta robots or `X-Robots-Tag`.
    pub noarchive: bool,
    /// A TDM reservation (`tdm-reservation: 1` header, TDMRep `<link>` or
    /// `/.well-known/tdmrep.json`).
    pub tdm_reserved: bool,
    /// `Content-Signal: ai-input=no`.
    pub ai_input_no: bool,
    /// robots.txt blocks the major AI crawlers (GPTBot, ClaudeBot, CCBot, Google-Extended) but
    /// allows `*`: treated as an AI reservation.
    pub ai_bot_blocked: bool,
    /// `Content-Signal: ai-train=no` (recorded; we never train).
    pub ai_train_no: bool,
}

impl UseSignals {
    /// The union of two signal sets (a signal present in either is present).
    pub fn union(&self, o: &UseSignals) -> UseSignals {
        UseSignals {
            noindex: self.noindex || o.noindex,
            noarchive: self.noarchive || o.noarchive,
            tdm_reserved: self.tdm_reserved || o.tdm_reserved,
            ai_input_no: self.ai_input_no || o.ai_input_no,
            ai_bot_blocked: self.ai_bot_blocked || o.ai_bot_blocked,
            ai_train_no: self.ai_train_no || o.ai_train_no,
        }
    }

    /// Whether any AI reservation is present (TDM, `ai-input=no`, AI-bot block).
    pub fn ai_reserved(&self) -> bool {
        self.tdm_reserved || self.ai_input_no || self.ai_bot_blocked
    }
}

/// How raw bytes may be kept. `Ephemeral < Archive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawUse {
    /// Change detection only: stored under `ephemeral/`, purged after 30 days, never Coldline.
    Ephemeral,
    /// Archived normally under `segments/`.
    Archive,
}

/// What may be indexed and served by the read API. `None < TitleAndLink < Full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexUse {
    /// Not indexed or served.
    None,
    /// Only title and link.
    TitleAndLink,
    /// Fully indexed (FTS) and served.
    Full,
}

/// What may be served to Voyager's agents. `LinkOnly < FactsOnly < Full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServeUse {
    /// A link only.
    LinkOnly,
    /// Extracted facts (+ link), no text.
    FactsOnly,
    /// Facts, excerpt and link.
    Full,
}

/// The uses allowed for a capture or a derived record. Stored as JSON in
/// `captures.use_policy_fetch` / `use_policy_final` and `record_versions.use_policy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsePolicy {
    /// Raw retention.
    pub raw: RawUse,
    /// Index / API serving.
    pub index: IndexUse,
    /// LLM extraction allowed.
    pub llm: bool,
    /// Embedding allowed.
    pub embed: bool,
    /// Serving to Voyager's agents.
    pub serve: ServeUse,
    /// Training allowed by the site (recorded only; we never train).
    pub train: bool,
}

impl UsePolicy {
    /// Everything allowed (no signals). The top of the lattice.
    pub const ALLOW_ALL: UsePolicy = UsePolicy {
        raw: RawUse::Archive,
        index: IndexUse::Full,
        llm: true,
        embed: true,
        serve: ServeUse::Full,
        train: true,
    };

    /// The most restrictive policy. The bottom of the lattice.
    pub const DENY_ALL: UsePolicy = UsePolicy {
        raw: RawUse::Ephemeral,
        index: IndexUse::None,
        llm: false,
        embed: false,
        serve: ServeUse::LinkOnly,
        train: false,
    };

    /// The greatest lower bound: the minimum of every dimension.
    pub fn meet(&self, o: &UsePolicy) -> UsePolicy {
        UsePolicy {
            raw: self.raw.min(o.raw),
            index: self.index.min(o.index),
            llm: self.llm && o.llm,
            embed: self.embed && o.embed,
            serve: self.serve.min(o.serve),
            train: self.train && o.train,
        }
    }

    /// Whether `self ≤ o` in every dimension (`self` allows nothing that `o` forbids).
    pub fn is_at_most(&self, o: &UsePolicy) -> bool {
        self.meet(o) == *self
    }

    /// The policy for a set of signals (the matrix in the module docs): the meet of the row
    /// of every signal present.
    pub fn from_signals(s: &UseSignals) -> UsePolicy {
        let mut p = UsePolicy::ALLOW_ALL;
        if s.noindex {
            p = p.meet(&UsePolicy {
                index: IndexUse::None,
                embed: false,
                serve: ServeUse::FactsOnly,
                ..UsePolicy::ALLOW_ALL
            });
        }
        if s.noarchive {
            p = p.meet(&UsePolicy {
                raw: RawUse::Ephemeral,
                ..UsePolicy::ALLOW_ALL
            });
        }
        if s.ai_reserved() {
            p = p.meet(&UsePolicy {
                index: IndexUse::TitleAndLink,
                llm: false,
                embed: false,
                serve: ServeUse::LinkOnly,
                ..UsePolicy::ALLOW_ALL
            });
        }
        if s.ai_train_no {
            p = p.meet(&UsePolicy {
                train: false,
                ..UsePolicy::ALLOW_ALL
            });
        }
        p
    }
}

impl Default for UsePolicy {
    fn default() -> Self {
        UsePolicy::ALLOW_ALL
    }
}

impl PartialOrd for UsePolicy {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering::*;
        match (self.is_at_most(o), o.is_at_most(self)) {
            (true, true) => Some(Equal),
            (true, false) => Some(Less),
            (false, true) => Some(Greater),
            (false, false) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_rows() {
        let noindex = UsePolicy::from_signals(&UseSignals {
            noindex: true,
            ..Default::default()
        });
        assert_eq!(noindex.index, IndexUse::None);
        assert!(noindex.llm && !noindex.embed);
        assert_eq!(noindex.serve, ServeUse::FactsOnly);

        let noarchive = UsePolicy::from_signals(&UseSignals {
            noarchive: true,
            ..Default::default()
        });
        assert_eq!(noarchive.raw, RawUse::Ephemeral);
        assert_eq!(noarchive.serve, ServeUse::Full);

        for s in [
            UseSignals {
                tdm_reserved: true,
                ..Default::default()
            },
            UseSignals {
                ai_input_no: true,
                ..Default::default()
            },
            UseSignals {
                ai_bot_blocked: true,
                ..Default::default()
            },
        ] {
            let p = UsePolicy::from_signals(&s);
            assert_eq!(p.index, IndexUse::TitleAndLink);
            assert!(!p.llm && !p.embed);
            assert_eq!(p.serve, ServeUse::LinkOnly);
            assert_eq!(p.raw, RawUse::Archive);
        }
        assert_eq!(
            UsePolicy::from_signals(&UseSignals::default()),
            UsePolicy::ALLOW_ALL
        );
    }

    #[test]
    fn serde_shape() {
        let j = serde_json::to_value(UsePolicy::ALLOW_ALL).unwrap();
        assert_eq!(
            j,
            serde_json::json!({"raw":"archive","index":"full","llm":true,"embed":true,"serve":"full","train":true})
        );
    }
}
