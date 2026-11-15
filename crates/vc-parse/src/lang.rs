//! Language: the declared tag (`<html lang>`, feed `<language>`) when valid, else whatlang on the
//! main text. Output is a lower-case BCP 47 primary language subtag (`en`, `pt`, `zh`).

/// Normalises a declared language tag to its primary subtag (`pt-BR` → `pt`).
pub(crate) fn primary_subtag(tag: &str) -> Option<String> {
    let p = tag.trim().split(['-', '_']).next()?.to_ascii_lowercase();
    let ok = (2..=3).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_lowercase());
    // `und`, `mul`, `zxx` mean "no language decided".
    (ok && !matches!(p.as_str(), "und" | "mul" | "zxx" | "mis")).then_some(p)
}

/// Detects the language of `text`; `None` when whatlang is not reliable.
pub(crate) fn detect(text: &str) -> Option<String> {
    // A prefix is plenty and bounds the cost.
    let mut end = text.len().min(4_096);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let info = whatlang::detect(&text[..end])?;
    if !info.is_reliable() {
        return None;
    }
    let code = info.lang().code();
    Some(iso639_3_to_1(code).unwrap_or(code).to_string())
}

/// ISO 639-3 → 639-1 for the languages whatlang knows (BCP 47 prefers the 2-letter form).
fn iso639_3_to_1(code: &str) -> Option<&'static str> {
    Some(match code {
        "afr" => "af",
        "aka" => "ak",
        "amh" => "am",
        "ara" => "ar",
        "aze" => "az",
        "bel" => "be",
        "ben" => "bn",
        "bul" => "bg",
        "cat" => "ca",
        "ces" => "cs",
        "cmn" => "zh",
        "cym" => "cy",
        "dan" => "da",
        "deu" => "de",
        "ell" => "el",
        "eng" => "en",
        "epo" => "eo",
        "est" => "et",
        "fin" => "fi",
        "fra" => "fr",
        "guj" => "gu",
        "heb" => "he",
        "hin" => "hi",
        "hrv" => "hr",
        "hun" => "hu",
        "hye" => "hy",
        "ind" => "id",
        "ita" => "it",
        "jav" => "jv",
        "jpn" => "ja",
        "kan" => "kn",
        "kat" => "ka",
        "khm" => "km",
        "kor" => "ko",
        "lat" => "la",
        "lav" => "lv",
        "lit" => "lt",
        "mal" => "ml",
        "mar" => "mr",
        "mkd" => "mk",
        "mya" => "my",
        "nep" => "ne",
        "nld" => "nl",
        "nob" => "nb",
        "ori" => "or",
        "pan" => "pa",
        "pes" => "fa",
        "pol" => "pl",
        "por" => "pt",
        "ron" => "ro",
        "rus" => "ru",
        "sin" => "si",
        "slk" => "sk",
        "slv" => "sl",
        "sna" => "sn",
        "spa" => "es",
        "srp" => "sr",
        "swe" => "sv",
        "tam" => "ta",
        "tel" => "te",
        "tgl" => "tl",
        "tha" => "th",
        "tuk" => "tk",
        "tur" => "tr",
        "ukr" => "uk",
        "urd" => "ur",
        "uzb" => "uz",
        "vie" => "vi",
        "yid" => "yi",
        "zul" => "zu",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtags() {
        assert_eq!(primary_subtag("pt-BR").as_deref(), Some("pt"));
        assert_eq!(primary_subtag(" EN_us ").as_deref(), Some("en"));
        assert_eq!(primary_subtag("und"), None);
        assert_eq!(primary_subtag("e"), None);
        assert_eq!(primary_subtag("12"), None);
        assert_eq!(primary_subtag(""), None);
    }

    #[test]
    fn every_whatlang_language_maps_to_two_letters() {
        for l in whatlang::Lang::all() {
            assert!(iso639_3_to_1(l.code()).is_some(), "{}", l.code());
        }
    }
}
