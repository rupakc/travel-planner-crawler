//! Small, pure helpers for the HTML fragments that JSON APIs embed (GOV.UK `govspeak` bodies,
//! Global Affairs Canada notes): text of a fragment, headings, links and simple tables.
//! Regex-based on purpose: these fragments are machine-generated and regular, and nothing here
//! is used for anything but reading them.

use std::sync::LazyLock;

use regex::Regex;

static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").expect("regex"));
static BREAK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>|</(?:p|li|tr|div|h[1-6]|dd|dt)>").expect("regex"));
static ENTITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[a-zA-Z]{2,8});").expect("regex")
});
static BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<h([1-6])\b[^>]*>(.*?)</h[1-6]>|<table\b[^>]*>(.*?)</table>").expect("regex")
});
static ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<tr\b[^>]*>(.*?)</tr>").expect("regex"));
static CELL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<(td|th)\b[^>]*>(.*?)</t[dh]>").expect("regex"));
static HREF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<a\b[^>]*?\bhref\s*=\s*"([^"]*)""#).expect("regex"));

/// Decodes the HTML entities these fragments use (named basics and numeric references);
/// unknown entities are kept as written.
pub fn decode_entities(s: &str) -> String {
    ENTITY
        .replace_all(s, |c: &regex::Captures<'_>| {
            let e = &c[1];
            let ch = if let Some(hex) = e.strip_prefix("#x").or_else(|| e.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = e.strip_prefix('#') {
                dec.parse::<u32>().ok().and_then(char::from_u32)
            } else {
                match e {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    "ndash" => Some('–'),
                    "mdash" => Some('—'),
                    "lsquo" => Some('‘'),
                    "rsquo" => Some('’'),
                    "ldquo" => Some('“'),
                    "rdquo" => Some('”'),
                    "hellip" => Some('…'),
                    "eacute" => Some('é'),
                    "ccedil" => Some('ç'),
                    _ => None,
                }
            };
            ch.map(String::from).unwrap_or_else(|| c[0].to_string())
        })
        .into_owned()
}

/// The text of an HTML fragment: block ends and `<br>` become line breaks, tags are removed,
/// entities decoded, spaces collapsed within each line, empty lines dropped.
pub fn text(fragment: &str) -> String {
    let t = BREAK.replace_all(fragment, "\n");
    let t = TAG.replace_all(&t, " ");
    let t = decode_entities(&t);
    t.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of a fragment on one line.
pub fn one_line(fragment: &str) -> String {
    text(fragment).lines().collect::<Vec<_>>().join(" ")
}

/// Every `href` of an `<a>` in the fragment, entity-decoded.
pub fn hrefs(fragment: &str) -> Vec<String> {
    HREF.captures_iter(fragment)
        .map(|c| decode_entities(c[1].trim()))
        .collect()
}

/// One table cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The cell's inner HTML.
    pub html: String,
    /// Its text ([`text`]).
    pub text: String,
}

/// One table with the heading it appears under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// Text of the closest heading before the table.
    pub section: Option<String>,
    /// Header cell texts (the first row of `<th>` cells), lower-cased.
    pub headers: Vec<String>,
    /// Body rows (rows with at least one `<td>`).
    pub rows: Vec<Vec<Cell>>,
}

/// The tables of a fragment, in order.
pub fn tables(fragment: &str) -> Vec<Table> {
    let mut out = Vec::new();
    let mut section: Option<String> = None;
    for b in BLOCK.captures_iter(fragment) {
        if let Some(h) = b.get(2) {
            section = Some(one_line(h.as_str())).filter(|s| !s.is_empty());
            continue;
        }
        let Some(t) = b.get(3) else { continue };
        let mut headers = Vec::new();
        let mut rows = Vec::new();
        for r in ROW.captures_iter(t.as_str()) {
            let cells: Vec<(bool, &str)> = CELL
                .captures_iter(r.get(1).map_or("", |m| m.as_str()))
                .map(|c| {
                    (
                        c[1].eq_ignore_ascii_case("th"),
                        c.get(2).map_or("", |m| m.as_str()),
                    )
                })
                .collect();
            if cells.is_empty() {
                continue;
            }
            if headers.is_empty() && rows.is_empty() && cells.iter().all(|(th, _)| *th) {
                headers = cells
                    .iter()
                    .map(|(_, h)| one_line(h).to_lowercase())
                    .collect();
                continue;
            }
            rows.push(
                cells
                    .iter()
                    .map(|(_, h)| Cell {
                        html: (*h).to_string(),
                        text: text(h),
                    })
                    .collect(),
            );
        }
        out.push(Table {
            section: section.clone(),
            headers,
            rows,
        });
    }
    out
}

/// The paragraphs under the first heading whose text matches `heading` (until the next
/// heading), as text.
pub fn under_heading(fragment: &str, heading: &Regex) -> Vec<String> {
    static HEADING: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<h[1-6]\b[^>]*>(.*?)</h[1-6]>").expect("regex"));
    let hs: Vec<_> = HEADING.captures_iter(fragment).collect();
    for (i, h) in hs.iter().enumerate() {
        if !heading.is_match(&one_line(&h[1])) {
            continue;
        }
        let start = h.get(0).map_or(0, |m| m.end());
        let end = hs
            .get(i + 1)
            .and_then(|n| n.get(0))
            .map_or(fragment.len(), |m| m.start());
        return text(&fragment[start..end])
            .lines()
            .map(str::to_string)
            .collect();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_entities() {
        assert_eq!(
            text("<p>A &amp; B&nbsp;&#8211; C<br>D</p><p>E&rsquo;s</p>"),
            "A & B – C\nD\nE’s"
        );
        assert_eq!(decode_entities("&unknown; &#x41;"), "&unknown; A");
    }

    #[test]
    fn tables_with_sections() {
        let h = r#"<h2>Tirana</h2><table><thead><tr><th>Name</th><th>Telephone</th></tr></thead>
            <tbody><tr><td><a href="https://x.example/">X Hospital</a></td><td>00355 1</td></tr>
            </tbody></table><h3>Durres</h3><table><tr><th>Name</th></tr><tr><td>Y</td></tr></table>"#;
        let t = tables(h);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].section.as_deref(), Some("Tirana"));
        assert_eq!(t[0].headers, ["name", "telephone"]);
        assert_eq!(t[0].rows[0][0].text, "X Hospital");
        assert_eq!(hrefs(&t[0].rows[0][0].html), ["https://x.example/"]);
        assert_eq!(t[1].section.as_deref(), Some("Durres"));
        assert_eq!(t[1].rows.len(), 1);
    }

    #[test]
    fn paragraphs_under_a_heading() {
        let re = Regex::new("(?i)hours").unwrap();
        let h =
            "<h2>Opening hours</h2><p>Mon–Fri 9–5</p><p>Closed weekends</p><h2>Other</h2><p>x</p>";
        assert_eq!(under_heading(h, &re), ["Mon–Fri 9–5", "Closed weekends"]);
        assert!(under_heading("<p>no headings</p>", &re).is_empty());
    }
}
