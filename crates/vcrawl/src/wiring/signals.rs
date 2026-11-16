//! Opt-out signals from response **headers** (spec §5.2 UsePolicy; body signals such as meta
//! robots are the processor's): `X-Robots-Tag` (for all robots or for `VoyagerBot`),
//! `tdm-reservation: 1` and `Content-Signal`.

use http::HeaderMap;
use vc_core::ROBOTS_TOKEN;
use vc_core::usepolicy::UseSignals;

/// The header signals of a response.
pub fn header_signals(h: &HeaderMap) -> UseSignals {
    let mut s = UseSignals::default();
    for v in h.get_all("x-robots-tag") {
        let Ok(v) = v.to_str() else { continue };
        // `<bot>: directives` applies to that bot only; bare directives apply to all.
        let directives = match v.split_once(':') {
            Some((bot, rest)) if !bot.contains(',') && !bot.trim().contains(' ') => {
                let bot = bot.trim();
                if bot.eq_ignore_ascii_case(ROBOTS_TOKEN) || bot == "*" {
                    rest
                } else if is_directive(bot) {
                    v // e.g. "unavailable_after: <date>"
                } else {
                    continue;
                }
            }
            _ => v,
        };
        for d in directives.split(',').map(|d| d.trim().to_ascii_lowercase()) {
            match d.as_str() {
                "noindex" | "none" => {
                    s.noindex = true;
                    if d == "none" {
                        s.noarchive = true;
                    }
                }
                "noarchive" => s.noarchive = true,
                _ => {}
            }
        }
    }
    if h.get_all("tdm-reservation")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.trim() == "1")
    {
        s.tdm_reserved = true;
    }
    for v in h.get_all("content-signal") {
        let Ok(v) = v.to_str() else { continue };
        for part in v.split([',', ';']) {
            let Some((k, val)) = part.split_once('=') else {
                continue;
            };
            let no = val.trim().eq_ignore_ascii_case("no");
            match k.trim().to_ascii_lowercase().as_str() {
                "ai-input" if no => s.ai_input_no = true,
                "ai-train" if no => s.ai_train_no = true,
                _ => {}
            }
        }
    }
    s
}

fn is_directive(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "unavailable_after" | "max-snippet" | "max-image-preview" | "max-video-preview"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.append(
                http::HeaderName::from_bytes(k.as_bytes()).expect("name"),
                v.parse().expect("value"),
            );
        }
        m
    }

    #[test]
    fn x_robots_tag_for_all_and_for_us_only() {
        let s = header_signals(&h(&[("x-robots-tag", "noindex, noarchive")]));
        assert!(s.noindex && s.noarchive);
        let s = header_signals(&h(&[("x-robots-tag", "VoyagerBot: noarchive")]));
        assert!(s.noarchive && !s.noindex);
        let s = header_signals(&h(&[("x-robots-tag", "otherbot: noindex")]));
        assert_eq!(s, UseSignals::default());
        let s = header_signals(&h(&[("x-robots-tag", "none")]));
        assert!(s.noindex && s.noarchive);
        let s = header_signals(&h(&[(
            "x-robots-tag",
            "unavailable_after: 25 Jun 2030 15:00:00 PST",
        )]));
        assert_eq!(s, UseSignals::default());
    }

    #[test]
    fn tdm_and_content_signal() {
        let s = header_signals(&h(&[
            ("tdm-reservation", "1"),
            ("content-signal", "search=yes, ai-input=no, ai-train=no"),
        ]));
        assert!(s.tdm_reserved && s.ai_input_no && s.ai_train_no);
        assert!(s.ai_reserved());
        let s = header_signals(&h(&[("tdm-reservation", "0")]));
        assert!(!s.tdm_reserved);
    }
}
