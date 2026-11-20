//! Licence and use-policy shaping of served data (spec §5.9, §5.11).
//!
//! The read model is expected to filter already; the API enforces the rules again so a
//! reader bug can never leak data:
//!
//! * classes the client may not receive, and `restricted`, are dropped;
//! * `index: none` items are dropped; `title_and_link` and `serve: link_only` keep title,
//!   licence and provenance (the link) only; `serve: facts_only` drops the excerpt;
//! * every excerpt is cut to `min(excerpt_chars, 300)` characters;
//! * for `facts_only` items, the title and every string inside `data` are cut to that length
//!   too, and full-text keys (`text`, `body`, `content`, `html`, `full_text`, `article_body`,
//!   `articleBody`) are removed, so no text beyond the excerpt is ever served.

use vc_core::index::{Item, RecordVersionView};
use vc_core::licence::LicenceClass;
use vc_core::records::MAX_EXCERPT_CHARS;
use vc_core::usepolicy::{IndexUse, ServeUse};

use crate::dto::{
    ItemDto, LicenceClassDto, LicenceDto, ProvenanceDto, RecordVersionDto, UsePolicyDto,
};

/// Keys that hold full text; never served for `facts_only`.
const FULL_TEXT_KEYS: &[&str] = &[
    "text",
    "body",
    "content",
    "html",
    "full_text",
    "article_body",
    "articleBody",
];

/// Licence rules for one client.
#[derive(Debug, Clone)]
pub struct Shaper {
    allowed: Vec<LicenceClass>,
    excerpt_max: usize,
}

/// The first `n` characters of `s` (on a char boundary).
pub fn truncate_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

fn clamp_strings(v: &mut serde_json::Value, n: usize) {
    match v {
        serde_json::Value::String(s) => {
            if s.chars().count() > n {
                *s = truncate_chars(s, n);
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| clamp_strings(x, n)),
        serde_json::Value::Object(o) => {
            o.retain(|k, _| !FULL_TEXT_KEYS.contains(&k.as_str()));
            o.values_mut().for_each(|x| clamp_strings(x, n));
        }
        _ => {}
    }
}

impl Shaper {
    /// Rules for a client allowed `allowed` classes, with the configured excerpt length.
    pub fn new(allowed: &[LicenceClass], excerpt_chars: usize) -> Self {
        Shaper {
            allowed: allowed
                .iter()
                .copied()
                .filter(LicenceClass::ingestible)
                .collect(),
            excerpt_max: excerpt_chars.min(MAX_EXCERPT_CHARS),
        }
    }

    /// The longest excerpt this client is served (≤ 300 characters).
    pub fn excerpt_max(&self) -> usize {
        self.excerpt_max
    }

    /// The classes this client may receive.
    pub fn allowed(&self) -> &[LicenceClass] {
        &self.allowed
    }

    /// Whether this client may receive `class`.
    pub fn permits(&self, class: LicenceClass) -> bool {
        class.ingestible() && self.allowed.contains(&class)
    }

    /// Shapes an item; `None` when it may not be served to this client.
    pub fn item(&self, it: &Item) -> Option<ItemDto> {
        if !self.permits(it.licence.class) {
            return None;
        }
        let licence = LicenceDto::from_core(&it.licence)?;
        let use_policy = UsePolicyDto::from_core(&it.use_policy)?;
        let link_only = it.use_policy.index == IndexUse::TitleAndLink
            || it.use_policy.serve == ServeUse::LinkOnly;
        let facts_only_class = it.licence.class == LicenceClass::FactsOnly;

        let mut title = it.title.clone();
        let mut excerpt = it
            .excerpt
            .as_deref()
            .map(|e| truncate_chars(e, self.excerpt_max));
        let mut data = it.data.clone();
        if link_only {
            excerpt = None;
            data = serde_json::json!({});
        } else if it.use_policy.serve == ServeUse::FactsOnly {
            excerpt = None;
        }
        if facts_only_class {
            title = title.map(|t| truncate_chars(&t, self.excerpt_max));
            clamp_strings(&mut data, self.excerpt_max);
        }
        Some(ItemDto {
            id: it.id.clone(),
            kind: it.kind.0.to_string(),
            schema: it.schema.clone(),
            title,
            excerpt,
            data,
            place_ids: it.place_ids.clone(),
            licence,
            provenance: it.provenance.iter().map(ProvenanceDto::from).collect(),
            conflict: it.conflict,
            use_policy,
            updated_at: it.updated_at,
            score: it.score,
        })
    }

    /// Shapes a record version; `None` when it may not be served to this client.
    pub fn version(&self, v: &RecordVersionView) -> Option<RecordVersionDto> {
        if !self.permits(v.licence_class) {
            return None;
        }
        let licence_class = LicenceClassDto::from_core(v.licence_class)?;
        let use_policy = UsePolicyDto::from_core(&v.use_policy)?;
        let mut data = v.data.clone();
        if v.use_policy.index == IndexUse::TitleAndLink || v.use_policy.serve == ServeUse::LinkOnly
        {
            data = serde_json::json!({});
        }
        if v.licence_class == LicenceClass::FactsOnly {
            clamp_strings(&mut data, self.excerpt_max);
        }
        Some(RecordVersionDto {
            record_id: v.record_id.0,
            version: v.version,
            schema: v.schema.clone(),
            data,
            data_hash: v.data_hash.clone(),
            captured_at: v.captured_at,
            extractor: (&v.extractor).into(),
            licence_class,
            use_policy,
            valid_from: v.valid_from,
            superseded_at: v.superseded_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("hi", 10), "hi");
        assert_eq!(truncate_chars("", 0), "");
    }

    #[test]
    fn clamp_drops_full_text_and_cuts_strings() {
        let mut v =
            serde_json::json!({"body": "x", "a": ["abcdef"], "b": {"content": 1, "c": "abc"}});
        clamp_strings(&mut v, 3);
        assert_eq!(v, serde_json::json!({"a": ["abc"], "b": {"c": "abc"}}));
    }

    #[test]
    fn restricted_never_permitted() {
        let s = Shaper::new(&[LicenceClass::Restricted, LicenceClass::Open], 1000);
        assert!(!s.permits(LicenceClass::Restricted));
        assert!(s.permits(LicenceClass::Open));
        assert_eq!(s.allowed(), &[LicenceClass::Open]);
        assert_eq!(s.excerpt_max, MAX_EXCERPT_CHARS);
    }
}
