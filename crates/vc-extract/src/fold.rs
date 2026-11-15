//! Name folding for place lookup and text grounding. Mirrors `fold()` in
//! `scripts/export_places.py` (same generated character table), so query keys match the
//! folded keys in `data/places.tsv.gz`.

use crate::fold_table::FOLD_TABLE;

fn table(c: char) -> Option<&'static str> {
    FOLD_TABLE
        .binary_search_by_key(&c, |(k, _)| *k)
        .ok()
        .map(|i| FOLD_TABLE[i].1)
}

fn is_quote(c: char) -> bool {
    matches!(
        c,
        '\'' | '\u{2019}'
            | '\u{2018}'
            | '\u{2bc}'
            | '\u{2bb}'
            | '`'
            | '\u{2b9}'
            | '\u{201c}'
            | '\u{201d}'
            | '"'
            | '\u{b4}'
    )
}

fn is_spacer(c: char) -> bool {
    matches!(
        c,
        '.' | '\u{b7}' | '-' | '\u{2010}'
            ..='\u{2015}' | '\u{2212}' | '_' | '/' | ',' | ';' | ':' | '(' | ')' | '[' | ']'
    )
}

fn is_mark_or_format(c: char) -> bool {
    matches!(c,
        '\u{300}'..='\u{36f}' | '\u{1ab0}'..='\u{1aff}' | '\u{1dc0}'..='\u{1dff}'
        | '\u{20d0}'..='\u{20ff}' | '\u{fe20}'..='\u{fe2f}'
        | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{feff}')
}

/// Folds a name: accents stripped (ß→ss, ø→o, …), lower-cased, dots/dashes/punctuation to
/// spaces, quotes and invisible characters dropped, whitespace collapsed.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if let Some(m) = table(c) {
            out.push_str(m);
            continue;
        }
        let mut low = c.to_lowercase();
        let l = match (low.next(), low.next()) {
            (Some(l), None) => l,
            _ => c,
        };
        if let Some(m) = table(l) {
            out.push_str(m);
        } else if is_mark_or_format(c) || is_quote(c) {
            continue;
        } else if is_spacer(c) || c.is_whitespace() {
            out.push(' ');
        } else if l.is_alphanumeric() {
            out.push(l);
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::fold;

    #[test]
    fn folds_like_the_export_script() {
        assert_eq!(fold("Zürich"), "zurich");
        assert_eq!(fold("Straße"), "strasse");
        assert_eq!(fold("  São   Paulo "), "sao paulo");
        assert_eq!(fold("Saint-Étienne"), "saint etienne");
        assert_eq!(fold("Boneh-ye ‘Alvān"), "boneh ye alvan");
        assert_eq!(fold("Marsá Maţrūḩ"), "marsa matruh");
        assert_eq!(fold("Hà Nội"), "ha noi");
        assert_eq!(fold("Tromsø"), "tromso");
        assert_eq!(fold("St. John's"), "st johns");
        assert_eq!(fold("東京"), "東京");
    }

    #[test]
    fn table_is_sorted() {
        let t = crate::fold_table::FOLD_TABLE;
        assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
    }
}
