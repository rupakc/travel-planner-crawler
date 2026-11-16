//! A pre-parse nesting guard for HTML.
//!
//! html5ever's tree builder checks element scopes by scanning the stack of open elements, so a
//! hostile page of 100k nested `<div>`s costs O(depth²) before any post-parse cap could act.
//! This pass models the open-element stack with a light tokenizer (quoted attributes, comments,
//! raw-text elements, void and self-closing tags, and the common implied end tags of `p`, `li`,
//! `dt`/`dd`, table cells and options) and drops every tag, text and comment that would sit
//! deeper than `limit`. The model is approximate; the exact cap is applied after parsing
//! (`html::prune`), so `limit` carries a small margin.

use std::borrow::Cow;

/// Elements that never have content.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr", "keygen", "basefont", "bgsound", "frame",
];

/// Elements whose content is raw text up to the matching end tag.
const RAW: &[&str] = &[
    "script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "noscript",
];

/// Start tags that close an open `<p>`.
const CLOSES_P: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "details",
    "dialog",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "ul",
    "li",
    "dd",
    "dt",
    "listing",
    "plaintext",
];

/// How far down the modelled stack implied-end-tag searches look.
const SEARCH: usize = 64;

/// Returns `html` with everything nested deeper than `limit` (modelled) removed. Borrowed when
/// nothing was removed.
pub(crate) fn limit_depth(html: &str, limit: usize) -> Cow<'_, str> {
    let b = html.as_bytes();
    let mut out: Option<String> = None;
    let mut stack: Vec<String> = Vec::new();
    // Opens beyond `limit` (not kept on the stack).
    let mut overflow = 0usize;
    let mut i = 0;
    let mut copied_to = 0; // bytes of `html` already reflected in `out`

    // Marks html[start..end] as dropped.
    let drop = |start: usize, end: usize, copied_to: &mut usize, out: &mut Option<String>| {
        let o = out.get_or_insert_with(|| String::with_capacity(html.len()));
        o.push_str(&html[*copied_to..start]);
        *copied_to = end;
    };

    while i < b.len() {
        let Some(lt) = memchr(b'<', &b[i..]).map(|p| i + p) else {
            if overflow > 0 {
                drop(i, b.len(), &mut copied_to, &mut out);
            }
            break;
        };
        if overflow > 0 && lt > i {
            drop(i, lt, &mut copied_to, &mut out);
        }
        let rest = &b[lt + 1..];
        let (end, action) = match rest.first() {
            Some(b'!') if rest.starts_with(b"!--") => {
                let e = find(b, lt + 4, b"-->").map_or(b.len(), |p| p + 3);
                (e, Action::Leaf)
            }
            Some(b'!' | b'?') => (tag_end(b, lt + 1), Action::Leaf),
            Some(b'/') => {
                let (name, _) = tag_name(b, lt + 2);
                if name.is_empty() {
                    (tag_end(b, lt + 2), Action::Leaf)
                } else {
                    (tag_end(b, lt + 2), Action::Close(name))
                }
            }
            Some(c) if c.is_ascii_alphabetic() => {
                let (name, _) = tag_name(b, lt + 1);
                let e = tag_end(b, lt + 1);
                let self_closing = e >= 2 && b[e - 1] == b'>' && b[e - 2] == b'/';
                if RAW.contains(&name.as_str()) {
                    // The whole element, through its end tag, is one leaf.
                    let close = format!("</{name}");
                    let e2 = find_ci(b, e, close.as_bytes()).map_or(b.len(), |p| tag_end(b, p + 2));
                    (e2, Action::Leaf)
                } else if VOID.contains(&name.as_str()) || self_closing {
                    (e, Action::Leaf)
                } else {
                    (e, Action::Open(name))
                }
            }
            // A bare `<` is text.
            _ => (lt + 1, Action::Leaf),
        };
        match action {
            Action::Leaf => {
                if overflow > 0 || stack.len() >= limit {
                    drop(lt, end, &mut copied_to, &mut out);
                }
            }
            Action::Open(name) => {
                if overflow == 0 {
                    implied_ends(&mut stack, &name);
                }
                if overflow > 0 || stack.len() >= limit {
                    overflow += 1;
                    drop(lt, end, &mut copied_to, &mut out);
                } else {
                    stack.push(name);
                }
            }
            Action::Close(name) => {
                if overflow > 0 {
                    overflow -= 1;
                    drop(lt, end, &mut copied_to, &mut out);
                } else if let Some(pos) = stack.iter().rposition(|n| *n == name) {
                    stack.truncate(pos);
                }
            }
        }
        i = end;
    }
    match out {
        None => Cow::Borrowed(html),
        Some(mut o) => {
            o.push_str(&html[copied_to.min(html.len())..]);
            Cow::Owned(o)
        }
    }
}

enum Action {
    Leaf,
    Open(String),
    Close(String),
}

/// Pops the elements a start tag implicitly closes (a simplified tree-builder model).
fn implied_ends(stack: &mut Vec<String>, name: &str) {
    let close_to = |stack: &mut Vec<String>, targets: &[&str], boundaries: &[&str]| {
        let lo = stack.len().saturating_sub(SEARCH);
        for idx in (lo..stack.len()).rev() {
            let n = stack[idx].as_str();
            if targets.contains(&n) {
                stack.truncate(idx);
                return;
            }
            if boundaries.contains(&n) {
                return;
            }
        }
    };
    const SCOPE: &[&str] = &[
        "button", "table", "td", "th", "caption", "template", "html", "object", "marquee", "applet",
    ];
    if CLOSES_P.contains(&name) {
        close_to(stack, &["p"], SCOPE);
    }
    match name {
        "li" => close_to(stack, &["li"], &["ul", "ol", "menu", "table", "html"]),
        "dt" | "dd" => close_to(stack, &["dt", "dd"], &["dl", "table", "html"]),
        "tr" => close_to(
            stack,
            &["tr"],
            &["table", "tbody", "thead", "tfoot", "html"],
        ),
        "td" | "th" => close_to(stack, &["td", "th"], &["tr", "table", "html"]),
        "tbody" | "thead" | "tfoot" => {
            close_to(stack, &["tbody", "thead", "tfoot"], &["table", "html"])
        }
        "option" => close_to(stack, &["option"], &["select", "datalist", "html"]),
        "optgroup" => close_to(stack, &["option", "optgroup"], &["select", "html"]),
        _ => {}
    }
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&c| c == needle)
}

fn find(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    if from >= b.len() {
        return None;
    }
    b[from..]
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|p| from + p)
}

fn find_ci(b: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    if from >= b.len() {
        return None;
    }
    b[from..]
        .windows(pat.len())
        .position(|w| w.eq_ignore_ascii_case(pat))
        .map(|p| from + p)
}

/// The lower-cased tag name starting at `i`, and the index after it.
fn tag_name(b: &[u8], i: usize) -> (String, usize) {
    let mut j = i;
    while j < b.len() && !matches!(b[j], b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'/' | b'>') {
        j += 1;
    }
    (String::from_utf8_lossy(&b[i..j]).to_ascii_lowercase(), j)
}

/// The index just after the `>` closing a tag that starts before `i`, honouring quoted
/// attribute values (a quote only opens right after `=`); the end of input if there is none.
fn tag_end(b: &[u8], i: usize) -> usize {
    let mut j = i;
    let mut after_eq = false;
    while j < b.len() {
        let c = b[j];
        if after_eq && (c == b'"' || c == b'\'') {
            match b[j + 1..].iter().position(|&x| x == c) {
                Some(p) => {
                    j += p + 2;
                    after_eq = false;
                    continue;
                }
                None => return b.len(),
            }
        }
        match c {
            b'>' => return j + 1,
            b'=' => after_eq = true,
            c if c.is_ascii_whitespace() => {}
            _ => after_eq = false,
        }
        j += 1;
    }
    b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shallow_documents_are_untouched() {
        let html = r#"<!doctype html><html><head><title>a < b</title><script>if (a<b) {}</script>
            </head><body><p>one<p>two<ul><li>a<li>b</ul><img src=x><br/>
            <div data-x="a>b">t</div><!-- <div><div> --></body></html>"#;
        assert!(matches!(limit_depth(html, 8), Cow::Borrowed(_)));
    }

    #[test]
    fn deep_nesting_is_cut() {
        let html = format!(
            "<div>a{}DEEP{}</div>b",
            "<div>".repeat(100),
            "</div>".repeat(100)
        );
        let out = limit_depth(&html, 10);
        assert!(!out.contains("DEEP"));
        assert!(out.starts_with("<div>a"));
        assert!(out.ends_with("</div>b"), "{out}");
        assert_eq!(out.matches("<div>").count(), 10);
    }

    #[test]
    fn unclosed_list_items_and_paragraphs_do_not_accumulate() {
        let html = format!("<ul>{}</ul><p>end", "<li><p>item".repeat(1_000));
        assert!(matches!(limit_depth(&html, 8), Cow::Borrowed(_)));
    }

    #[test]
    fn utf8_and_unterminated_input_survive() {
        for s in [
            "<div",
            "<div a='",
            "é<",
            "<!--",
            "<script>",
            "</",
            "<",
            "<a href=\"x>y\">é</a>",
        ] {
            let _ = limit_depth(s, 1);
            let _ = limit_depth(s, 0);
        }
        let s = format!("{}é", "<b>".repeat(5));
        assert!(limit_depth(&s, 2).is_char_boundary(0));
    }
}
