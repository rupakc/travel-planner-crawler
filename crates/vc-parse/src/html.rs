//! HTML: charset, a capped DOM, links, body signals, structured data, main text (dom_smoothie),
//! language, simhash and the SPA heuristic.

use std::collections::HashSet;

use dom_smoothie::{Config as SmoothieConfig, Readability, TextMode};
use scraper::{ElementRef, Html, Node, Selector};
use vc_core::Result;
use vc_core::media::{MediaType, MediaTypeMatch};
use vc_core::parse::{ContentHandler, Limits, Link, LinkKind, Parsed};
use vc_core::store::CaptureMeta;
use vc_core::url::CanonicalUrl;
use vc_core::usepolicy::UseSignals;

use crate::simhash::simhash64;
use crate::structured::{jsonld_from_html, microdata_from_html, select_all};
use crate::text::{Deadline, collapse_ws, decode, short_text, truncate_bytes};
use crate::{depth, lang};

/// Default for [`HtmlHandler::render_min_text_chars`] (`[parse] render_min_text_chars`).
pub const DEFAULT_RENDER_MIN_TEXT_CHARS: usize = 200;

/// Extra modelled depth allowed by the pre-parse guard (implied `html`/`body`/`tbody`…); the
/// exact cap is applied by [`prune`].
const DEPTH_MARGIN: usize = 8;

/// Element ids / attributes that mark a client-rendered app shell.
const SPA_ROOT_IDS: &[&str] = &["root", "app", "__next", "__nuxt", "___gatsby", "svelte"];

/// The HTML handler.
///
/// The DOM is pruned to `Limits::max_dom_depth` and `Limits::max_nodes` right after parsing
/// (deeper or later nodes are dropped), so every later stage sees a bounded tree.
#[derive(Debug, Clone, Copy)]
pub struct HtmlHandler {
    /// Below this many characters of visible text, a page with scripts or an app root is
    /// flagged `needs_render`.
    pub render_min_text_chars: usize,
}

impl Default for HtmlHandler {
    fn default() -> Self {
        HtmlHandler {
            render_min_text_chars: DEFAULT_RENDER_MIN_TEXT_CHARS,
        }
    }
}

impl ContentHandler for HtmlHandler {
    fn media(&self) -> &'static [MediaTypeMatch] {
        &[MediaTypeMatch::Is(MediaType::Html)]
    }

    fn handle(&self, meta: &CaptureMeta, body: &[u8], lim: &Limits) -> Result<Parsed> {
        let deadline = Deadline::new(lim.cpu_time);
        let text = decode(body, prescan_charset(body).as_deref());
        let text = depth::limit_depth(&text, lim.max_dom_depth as usize + DEPTH_MARGIN);
        let mut doc = Html::parse_document(&text);
        drop(text);
        prune(&mut doc, lim);
        deadline.check("html parse")?;

        let base = base_url(&doc, &meta.url);
        let (signals, nofollow) = body_signals(&doc);
        let links = if lim.max_links == 0 {
            Vec::new()
        } else {
            links(&doc, &meta.url, &base, nofollow, lim.max_links)
        };

        let mut structured = Vec::new();
        jsonld_from_html(&doc, &mut structured);
        microdata_from_html(&doc, &base, &mut structured);
        deadline.check("html structured")?;

        let visible = visible_text(&doc);
        let visible_chars = visible.chars().count();
        let main = if deadline.expired() {
            None
        } else {
            main_text(&doc, &meta.url, lim)
        };
        let main_text = main
            .map(|m| m.text)
            .filter(|t| !t.trim().is_empty())
            .or_else(|| (!visible.is_empty()).then(|| visible.clone()))
            .map(|t| truncate_bytes(t, lim.max_text_bytes));

        let title = select_first(&doc, "title")
            .map(|t| short_text(&t.text().collect::<String>()))
            .filter(|t| !t.is_empty());
        let lang = select_first(&doc, "html[lang]")
            .and_then(|h| h.value().attr("lang"))
            .and_then(lang::primary_subtag)
            .or_else(|| main_text.as_deref().and_then(lang::detect));
        let simhash = main_text.as_deref().map(simhash64);
        let needs_render = visible_chars < self.render_min_text_chars && looks_like_app(&doc);

        Ok(Parsed {
            title,
            main_text,
            lang,
            links,
            structured,
            simhash,
            body_signals: signals,
            needs_render,
        })
    }
}

/// Finds a `charset` declaration in the first 1 KiB (`<meta charset>` or `http-equiv`).
fn prescan_charset(body: &[u8]) -> Option<String> {
    let head = &body[..body.len().min(1024)];
    let lower = String::from_utf8_lossy(head).to_ascii_lowercase();
    let i = lower.find("charset")? + "charset".len();
    let rest = lower[i..].trim_start().strip_prefix('=')?.trim_start();
    let rest = rest.trim_start_matches(['"', '\'']);
    let label: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
        .collect();
    (!label.is_empty()).then_some(label)
}

/// Drops every node deeper than `max_dom_depth` and every node after the first `max_nodes`
/// (pre-order). Iterative, so a hostile nesting depth cannot overflow the stack.
fn prune(doc: &mut Html, lim: &Limits) {
    let max_depth = lim.max_dom_depth as usize;
    let max_nodes = lim.max_nodes as usize;
    let mut drop_ids = Vec::new();
    let mut kept = 0usize;
    let mut stack = vec![(doc.tree.root(), 0usize)];
    while let Some((n, depth)) = stack.pop() {
        if depth > max_depth || kept >= max_nodes {
            drop_ids.push(n.id());
            continue;
        }
        kept += 1;
        // Reverse so children are visited in document order.
        let children: Vec<_> = n.children().collect();
        for c in children.into_iter().rev() {
            stack.push((c, depth + 1));
        }
    }
    for id in drop_ids {
        if let Some(mut n) = doc.tree.get_mut(id) {
            n.detach();
        }
    }
}

fn select_first<'a>(doc: &'a Html, sel: &str) -> Option<ElementRef<'a>> {
    let s = Selector::parse(sel).ok()?;
    select_all(doc, &s).next()
}

fn base_url(doc: &Html, page: &CanonicalUrl) -> CanonicalUrl {
    select_first(doc, "base[href]")
        .and_then(|b| b.value().attr("href"))
        .and_then(|h| page.join(h).ok())
        .unwrap_or_else(|| page.clone())
}

/// Meta robots (`robots` or our `voyagerbot` token) and TDMRep. Returns the signals and whether
/// the page asks not to follow its links.
fn body_signals(doc: &Html) -> (UseSignals, bool) {
    let mut s = UseSignals::default();
    let mut nofollow = false;
    let metas = Selector::parse("meta[name][content]").expect("static selector");
    for m in select_all(doc, &metas) {
        let name = m
            .value()
            .attr("name")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let content = m.value().attr("content").unwrap_or_default();
        match name.as_str() {
            "robots" | "voyagerbot" => {
                for tok in content
                    .split([',', ' ', '\t', '\n'])
                    .map(|t| t.trim().to_ascii_lowercase())
                {
                    match tok.as_str() {
                        "noindex" => s.noindex = true,
                        "noarchive" => s.noarchive = true,
                        "nofollow" => nofollow = true,
                        "none" => {
                            s.noindex = true;
                            nofollow = true;
                        }
                        _ => {}
                    }
                }
            }
            "tdm-reservation" if content.trim() == "1" => s.tdm_reserved = true,
            _ => {}
        }
    }
    let links = Selector::parse("link[rel]").expect("static selector");
    if select_all(doc, &links).any(|l| {
        l.value()
            .attr("rel")
            .unwrap_or_default()
            .split_whitespace()
            .any(|r| r.eq_ignore_ascii_case("tdm-reservation"))
    }) {
        s.tdm_reserved = true;
    }
    (s, nofollow)
}

fn rel_tokens(el: &scraper::node::Element) -> Vec<String> {
    el.attr("rel")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect()
}

/// `<link>` (canonical, feeds, hreflang alternates) then `<a>`/`<area>` links, in document
/// order, deduplicated, capped. With page-level `nofollow`, only the canonical link is kept.
fn links(
    doc: &Html,
    page: &CanonicalUrl,
    base: &CanonicalUrl,
    nofollow: bool,
    max: usize,
) -> Vec<Link> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |out: &mut Vec<Link>, url: CanonicalUrl, anchor: String, kind, rel| {
        if out.len() >= max || (kind == LinkKind::Page && url == *page) {
            return;
        }
        if seen.insert((url.as_str().to_string(), kind)) {
            out.push(Link {
                url,
                anchor,
                kind,
                rel,
                lastmod: None,
            });
        }
    };

    let link_sel = Selector::parse("link[href][rel]").expect("static selector");
    for l in select_all(doc, &link_sel) {
        let el = l.value();
        let rel = rel_tokens(el);
        let ty = el
            .attr("type")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let kind = if rel.iter().any(|r| r == "canonical") {
            LinkKind::Canonical
        } else if rel.iter().any(|r| r == "alternate") {
            if matches!(
                ty.as_str(),
                "application/rss+xml" | "application/atom+xml" | "application/feed+json"
            ) {
                LinkKind::Feed
            } else if el.attr("hreflang").is_some() {
                LinkKind::Alternate
            } else {
                continue;
            }
        } else {
            continue;
        };
        if nofollow && kind != LinkKind::Canonical {
            continue;
        }
        let Some(url) = el.attr("href").and_then(|h| base.join(h).ok()) else {
            continue;
        };
        let anchor = short_text(el.attr("title").unwrap_or_default());
        push(&mut out, url, anchor, kind, rel);
    }
    if nofollow {
        return out;
    }

    let a_sel = Selector::parse("a[href], area[href]").expect("static selector");
    for a in select_all(doc, &a_sel) {
        if out.len() >= max {
            break;
        }
        let el = a.value();
        let Some(url) = el.attr("href").and_then(|h| base.join(h).ok()) else {
            continue;
        };
        let anchor = if el.name() == "area" {
            el.attr("alt").unwrap_or_default().to_string()
        } else {
            a.text().collect::<String>()
        };
        push(
            &mut out,
            url,
            short_text(&anchor),
            LinkKind::Page,
            rel_tokens(el),
        );
    }
    out
}

/// Visible text of the document (no script/style/noscript/template/head), whitespace-collapsed.
fn visible_text(doc: &Html) -> String {
    let mut s = String::new();
    let mut stack = vec![(doc.tree.root(), false)];
    while let Some((n, hidden)) = stack.pop() {
        match n.value() {
            Node::Text(t) if !hidden => {
                s.push_str(t);
                s.push(' ');
            }
            _ => {}
        }
        let hidden = hidden
            || n.value().as_element().is_some_and(|e| {
                matches!(
                    e.name(),
                    "script" | "style" | "noscript" | "template" | "head"
                )
            });
        let children: Vec<_> = n.children().collect();
        for c in children.into_iter().rev() {
            stack.push((c, hidden));
        }
    }
    collapse_ws(&s)
}

/// A shell page: scripts present, or an app root element.
fn looks_like_app(doc: &Html) -> bool {
    let scripts = Selector::parse("script").expect("static selector");
    if select_all(doc, &scripts).next().is_some() {
        return true;
    }
    let ids = Selector::parse("[id], [ng-app], [data-reactroot]").expect("static selector");
    select_all(doc, &ids).any(|e| {
        let v = e.value();
        v.attr("ng-app").is_some()
            || v.attr("data-reactroot").is_some()
            || v.attr("id").is_some_and(|id| SPA_ROOT_IDS.contains(&id))
    })
}

struct MainText {
    text: String,
}

/// Main content via dom_smoothie (Readability) on the pruned DOM. `None` when it gives up.
fn main_text(doc: &Html, url: &CanonicalUrl, lim: &Limits) -> Option<MainText> {
    let cfg = SmoothieConfig {
        max_elements_to_parse: lim.max_nodes as usize,
        disable_json_ld: true,
        text_mode: TextMode::Formatted,
        ..SmoothieConfig::default()
    };
    let mut r = Readability::new(doc.html(), Some(url.as_str()), Some(cfg)).ok()?;
    let article = r.parse().ok()?;
    let mut text = String::new();
    for line in article.text_content.lines() {
        let line = collapse_ws(line);
        if line.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&line);
    }
    Some(MainText { text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charset_prescan() {
        assert_eq!(
            prescan_charset(b"<meta charset=\"ISO-8859-1\">").as_deref(),
            Some("iso-8859-1")
        );
        assert_eq!(
            prescan_charset(
                b"<meta http-equiv=content-type content='text/html; charset=shift_jis'>"
            )
            .as_deref(),
            Some("shift_jis")
        );
        assert_eq!(prescan_charset(b"<p>no charset here</p>"), None);
        assert_eq!(prescan_charset(b"charset"), None);
    }
}
