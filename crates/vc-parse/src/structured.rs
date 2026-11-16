//! Structured data: JSON-LD (with `@graph` flattened) and microdata (as JSON-LD-like objects).

use scraper::{ElementRef, Html, Node, Selector};
use serde_json::{Map, Value};
use vc_core::url::CanonicalUrl;

use crate::text::collapse_ws;

/// Elements matching `sel` in the document **tree** (pruned subtrees excluded; `Html::select`
/// would still see detached nodes, which stay in the arena).
pub(crate) fn select_all<'a, 'b>(
    doc: &'a Html,
    sel: &'b Selector,
) -> impl Iterator<Item = ElementRef<'a>> + use<'a, 'b> {
    doc.tree
        .root()
        .children()
        .filter_map(ElementRef::wrap)
        .flat_map(move |top| {
            let me = sel.matches(&top).then_some(top);
            me.into_iter().chain(top.select(sel))
        })
}

/// Max structured objects kept per capture.
pub(crate) const MAX_STRUCTURED: usize = 1_000;

/// Flattens one JSON-LD document into top-level nodes: arrays are spread, and a `@graph` is
/// replaced by its members, each inheriting the container's `@context` when it has none.
pub(crate) fn flatten_jsonld(v: Value, out: &mut Vec<Value>) {
    match v {
        Value::Array(items) => {
            for i in items {
                if out.len() >= MAX_STRUCTURED {
                    return;
                }
                flatten_jsonld(i, out);
            }
        }
        Value::Object(mut obj) => match obj.remove("@graph") {
            Some(graph) => {
                let ctx = obj.get("@context").cloned();
                let mut nodes = Vec::new();
                flatten_jsonld(graph, &mut nodes);
                for mut n in nodes {
                    if let (Some(ctx), Value::Object(m)) = (&ctx, &mut n)
                        && !m.contains_key("@context")
                    {
                        m.insert("@context".into(), ctx.clone());
                    }
                    if out.len() < MAX_STRUCTURED {
                        out.push(n);
                    }
                }
            }
            None => {
                if out.len() < MAX_STRUCTURED {
                    out.push(Value::Object(obj));
                }
            }
        },
        // Scalars are not JSON-LD nodes.
        _ => {}
    }
}

/// Whether a JSON value looks like JSON-LD (an object or array of objects with `@context`,
/// `@type` or `@graph`).
pub(crate) fn looks_like_jsonld(v: &Value) -> bool {
    match v {
        Value::Object(m) => ["@context", "@type", "@graph"]
            .iter()
            .any(|k| m.contains_key(*k)),
        Value::Array(a) => !a.is_empty() && a.iter().all(looks_like_jsonld),
        _ => false,
    }
}

/// Every parseable `<script type="application/ld+json">` block, flattened. Broken blocks are
/// skipped (serde_json's recursion limit also bounds nesting).
pub(crate) fn jsonld_from_html(doc: &Html, out: &mut Vec<Value>) {
    let sel = Selector::parse("script").expect("static selector");
    for s in select_all(doc, &sel) {
        let is_ld = s.value().attr("type").is_some_and(|t| {
            t.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("application/ld+json")
        });
        if !is_ld {
            continue;
        }
        let raw: String = s.text().collect();
        // Some sites wrap the JSON in an HTML comment or CDATA.
        let raw = raw
            .trim()
            .trim_start_matches("<!--")
            .trim_end_matches("-->")
            .trim_start_matches("//<![CDATA[")
            .trim_end_matches("//]]>")
            .trim();
        if let Ok(v) = serde_json::from_str::<Value>(raw) {
            flatten_jsonld(v, out);
        }
    }
}

/// Microdata (WHATWG): every top-level item (an `itemscope` element that is not itself an
/// `itemprop` value) becomes `{"@context", "@type", props…}`. `itemref` is not followed.
pub(crate) fn microdata_from_html(doc: &Html, base: &CanonicalUrl, out: &mut Vec<Value>) {
    let sel = Selector::parse("[itemscope]").expect("static selector");
    for el in select_all(doc, &sel) {
        if out.len() >= MAX_STRUCTURED {
            return;
        }
        if el.value().attr("itemprop").is_none() {
            out.push(item(el, base));
        }
    }
}

fn item(el: ElementRef<'_>, base: &CanonicalUrl) -> Value {
    let mut m = Map::new();
    if let Some(types) = el.value().attr("itemtype") {
        let types: Vec<&str> = types.split_whitespace().collect();
        if let Some(first) = types.first() {
            // `https://schema.org/Place` → context `https://schema.org`, type `Place`.
            if let Some((ctx, _)) = first.trim_end_matches('/').rsplit_once('/') {
                m.insert("@context".into(), Value::String(ctx.to_string()));
            }
        }
        let names: Vec<Value> = types
            .iter()
            .map(|t| {
                let t = t.trim_end_matches('/');
                Value::String(t.rsplit(['/', '#']).next().unwrap_or(t).to_string())
            })
            .collect();
        match names.len() {
            0 => {}
            1 => {
                m.insert("@type".into(), names.into_iter().next().unwrap_or_default());
            }
            _ => {
                m.insert("@type".into(), Value::Array(names));
            }
        }
    }
    if let Some(id) = el.value().attr("itemid") {
        m.insert("@id".into(), Value::String(id.trim().to_string()));
    }
    collect_props(el, base, &mut m);
    Value::Object(m)
}

/// Walks the children of an item; a nested `itemscope` owns its own subtree.
fn collect_props(el: ElementRef<'_>, base: &CanonicalUrl, m: &mut Map<String, Value>) {
    for child in el.children().filter_map(ElementRef::wrap) {
        let v = child.value();
        if let Some(names) = v.attr("itemprop") {
            let val = prop_value(child, base);
            for name in names.split_whitespace() {
                add_prop(m, name, val.clone());
            }
        }
        if v.attr("itemscope").is_none() {
            collect_props(child, base, m);
        }
    }
}

fn add_prop(m: &mut Map<String, Value>, name: &str, val: Value) {
    match m.get_mut(name) {
        None => {
            m.insert(name.to_string(), val);
        }
        Some(Value::Array(a)) => a.push(val),
        Some(prev) => {
            let p = prev.take();
            *prev = Value::Array(vec![p, val]);
        }
    }
}

fn prop_value(el: ElementRef<'_>, base: &CanonicalUrl) -> Value {
    let v = el.value();
    if v.attr("itemscope").is_some() {
        return item(el, base);
    }
    let attr = |a: &str| v.attr(a).map(|s| s.trim().to_string());
    let url_attr = |a: &str| {
        v.attr(a).map(|s| {
            base.join(s)
                .map(|u| u.as_str().to_string())
                .unwrap_or_else(|_| s.trim().to_string())
        })
    };
    let s = match v.name() {
        "meta" => attr("content"),
        "a" | "area" | "link" => url_attr("href"),
        "img" | "audio" | "video" | "source" | "track" | "iframe" | "embed" => url_attr("src"),
        "object" => url_attr("data"),
        "data" | "meter" => attr("value"),
        "time" => attr("datetime"),
        _ => None,
    };
    Value::String(s.unwrap_or_else(|| collapse_ws(&text_of(el))))
}

/// The text of an element, without `<script>`/`<style>` contents.
pub(crate) fn text_of(el: ElementRef<'_>) -> String {
    let mut s = String::new();
    for n in el.descendants() {
        if let Node::Text(t) = n.value() {
            let hidden = n.ancestors().any(|a| {
                a.value()
                    .as_element()
                    .is_some_and(|e| matches!(e.name(), "script" | "style" | "template"))
            });
            if !hidden {
                s.push_str(t);
                s.push(' ');
            }
        }
    }
    s
}
