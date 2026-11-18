//! Phone numbers as sources write them → `{raw, e164}`.
//!
//! These numbers end up on a traveller's emergency card, so normalisation never guesses:
//! - `raw` is always kept exactly as written (trimmed);
//! - `e164` is set only when the number is written in international form (`+CC …`, `(+CC) …`
//!   or `00CC …`), contains nothing but digits and separators, has no extension, and has 8–15
//!   digits. The only rewrite is dropping a trunk `(0)` after the country code
//!   (`+44 (0)20 …` → `+4420…`), which is what that notation means.
//! - Local forms (`(02) 1234 5678`, `011 …`, `1-800-…`) get no `e164`: turning them into
//!   international form needs the trunk-prefix rules of each country, i.e. a guess.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

/// One number from a phone field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Phone {
    /// As written (trimmed).
    pub raw: String,
    /// E.164 (`+` and 8–15 digits), only when unambiguous.
    pub e164: Option<String>,
    /// A trailing parenthetical note of a single-number field, as written
    /// (`(852) 3719 4700 (this line is not for visas)` → `this line is not for visas`).
    pub note: Option<String>,
}

static TRAILING_NOTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*\d.*?)\s*\(([^()]*[A-Za-z][^()]*)\)\s*$").expect("regex"));

fn digits(s: &str) -> usize {
    s.bytes().filter(u8::is_ascii_digit).count()
}

/// E.164 of one number written in international form, if it is unambiguous.
pub fn e164(raw: &str) -> Option<String> {
    let s = raw.trim().replace(['\u{a0}', '\u{2009}', '\u{202f}'], " ");
    // Alternatives written as `4973/4/5`: the first number is the one written in full.
    let s = match s.split_once('/') {
        Some((first, rest))
            if rest.split('/').all(|p| {
                !p.trim().is_empty() && digits(p) == p.trim().len() && p.trim().len() <= 4
            }) =>
        {
            first.to_string()
        }
        Some(_) => return None,
        None => s,
    };
    if !s
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, ' ' | '-' | '.' | '(' | ')' | '+'))
    {
        return None; // letters (an extension, "Cell"), other symbols
    }
    let t = s.trim_start_matches('(').trim_start();
    let intl = t.strip_prefix('+').or_else(|| t.strip_prefix("00"))?;
    if intl.contains('+') {
        return None;
    }
    let cleaned = intl.replace("(0)", "");
    let d: String = cleaned.chars().filter(char::is_ascii_digit).collect();
    if !(8..=15).contains(&d.len()) || d.starts_with('0') {
        return None;
    }
    Some(format!("+{d}"))
}

/// Splits a phone field that may hold several numbers (`a / b`, `a; b`, `a // b`) into
/// numbers, each `raw` an exact slice of the field. A part with fewer than 6 digits is an
/// alternative ending or extension of the previous number (`4973/4/5`, `Ext. 3341 / 3342`) and
/// stays with it. Fields without any 6-digit number yield nothing.
pub fn parse(field: &str) -> Vec<Phone> {
    let field = field.trim();
    if let Some(c) = TRAILING_NOTE.captures(field) {
        let mut one = split(c.get(1).map_or("", |m| m.as_str()));
        if one.len() == 1 {
            one[0].note = Some(c[2].trim().to_string()).filter(|n| !n.is_empty());
            return one;
        }
    }
    split(field)
}

fn split(field: &str) -> Vec<Phone> {
    let field = field.trim();
    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    for (i, c) in field.char_indices() {
        if c == '/' || c == ';' {
            segs.push((start, i));
            start = i + c.len_utf8();
        }
    }
    segs.push((start, field.len()));
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for (s, e) in segs {
        let d = digits(&field[s..e]);
        if d >= 6 {
            groups.push((s, e));
        } else if d > 0
            && let Some(g) = groups.last_mut()
        {
            g.1 = e;
        }
    }
    groups
        .into_iter()
        .map(|(s, e)| field[s..e].trim().to_string())
        .filter(|raw| raw.chars().count() <= 64)
        .map(|raw| Phone {
            e164: e164(&raw),
            raw,
            note: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn international_forms_normalise() {
        assert_eq!(e164("+351 21 316 4600").as_deref(), Some("+351213164600"));
        assert_eq!(
            e164("+44 (0) 207 004 6000").as_deref(),
            Some("+442070046000")
        );
        assert_eq!(
            e164("+33 (0)1 44 43 29 02").as_deref(),
            Some("+33144432902")
        );
        assert_eq!(e164("+254 (20) 366-3000").as_deref(), Some("+254203663000"));
        assert_eq!(e164("(+973) 1753 6270").as_deref(), Some("+97317536270"));
        assert_eq!(e164("+ 250 252 554 800").as_deref(), Some("+250252554800"));
        assert_eq!(e164("00355 4235 7535").as_deref(), Some("+35542357535"));
        assert_eq!(e164("+355 4 223 4973/4/5").as_deref(), Some("+35542234973"));
    }

    #[test]
    fn local_or_unclear_forms_are_never_guessed() {
        for raw in [
            "(61) 2 6270 4000",
            "92 (51) 208-6000",
            "1-844-880-6519",
            "011 (509) 2812-9000",
            "218-3030",
            "+962 6 590 1500 Ext. 3341",
            "+235 66-23-73-89 (Cell)",
            "-",
            "–",
            "+12",
            "+0 123 456 789",
            "+44 20 7008 5000 +44 20 7008 5001",
        ] {
            assert_eq!(e164(raw), None, "{raw}");
        }
    }

    #[test]
    fn fields_split_into_numbers() {
        let p = parse("1-246-629-3550 / 1-844-880-6519");
        assert_eq!(
            p.iter().map(|p| p.raw.as_str()).collect::<Vec<_>>(),
            ["1-246-629-3550", "1-844-880-6519"]
        );
        assert!(p.iter().all(|p| p.e164.is_none()));
        let p = parse("+355 4 223 4973/4/5 // +44 20 7008 5000");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].raw, "+355 4 223 4973/4/5");
        assert_eq!(p[1].e164.as_deref(), Some("+442070085000"));
        assert!(parse("–").is_empty());
        let p = parse(
            "(852) 3719 4700 (this line is not for immigration/visa; ce numéro n'est pas pour l'immigration/visa)",
        );
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].raw, "(852) 3719 4700");
        assert_eq!(
            p[0].note.as_deref(),
            Some(
                "this line is not for immigration/visa; ce numéro n'est pas pour l'immigration/visa"
            )
        );
        let p = parse("(235) 22-51-33-34/(235) 66-23-73-89 (Cell)/(235) 66-93-52-60 (Ass't)");
        assert_eq!(
            p.iter().map(|p| p.raw.as_str()).collect::<Vec<_>>(),
            [
                "(235) 22-51-33-34",
                "(235) 66-23-73-89 (Cell)",
                "(235) 66-93-52-60 (Ass't)"
            ]
        );
        let p = parse("+7 (7172) 47 55 77 / 78 / 79 / 80");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].e164.as_deref(), Some("+77172475577"));
        assert!(parse("").is_empty());
        let p = parse("+962 6 590 1500 Ext. 3341 / 3342");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].raw, "+962 6 590 1500 Ext. 3341 / 3342");
        assert_eq!(p[0].e164, None);
    }
}
