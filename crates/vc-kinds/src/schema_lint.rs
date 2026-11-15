//! Structural rules for kind schemas (injection containment, r4; Vertex `responseSchema`
//! compatibility) and verbatim-pointer resolution.

use serde_json::Value;

/// The draft every kind schema declares.
pub const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// Keywords a kind schema may use: the subset Vertex `responseSchema` understands, plus
/// annotations. No `$ref`, combinators or conditionals.
const ALLOWED_KEYWORDS: &[&str] = &[
    "$schema",
    "$id",
    "title",
    "description",
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "pattern",
    "enum",
    "minimum",
    "maximum",
    "format",
];

/// Upper bound on concrete pointers one `verbatim` entry may expand to.
const MAX_EXPANSION: usize = 2_000;

/// Checks the structural rules; returns the first violation (with its schema path).
pub(crate) fn lint(schema: &Value) -> Result<(), String> {
    if schema.get("$schema").and_then(Value::as_str) != Some(DRAFT_2020_12) {
        return Err(format!("$schema must be {DRAFT_2020_12:?}"));
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("the root must be type: object".into());
    }
    node(schema, "#", true)
}

fn node(n: &Value, path: &str, root: bool) -> Result<(), String> {
    let obj = n
        .as_object()
        .ok_or_else(|| format!("{path}: a schema must be an object"))?;
    for k in obj.keys() {
        if !ALLOWED_KEYWORDS.contains(&k.as_str()) {
            return Err(format!(
                "{path}: keyword {k:?} is not allowed in kind schemas"
            ));
        }
        if !root && (k == "$schema" || k == "$id") {
            return Err(format!("{path}: {k} only at the root"));
        }
    }
    let ty = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{path}: every node needs a single string `type`"))?;
    match ty {
        "object" => {
            if obj.get("additionalProperties") != Some(&Value::Bool(false)) {
                return Err(format!("{path}: objects need additionalProperties: false"));
            }
            let props = obj
                .get("properties")
                .and_then(Value::as_object)
                .ok_or_else(|| format!("{path}: objects need properties"))?;
            if let Some(req) = obj.get("required") {
                let req = req
                    .as_array()
                    .ok_or_else(|| format!("{path}: required must be an array"))?;
                for r in req {
                    let r = r.as_str().unwrap_or_default();
                    if !props.contains_key(r) {
                        return Err(format!("{path}: required {r:?} is not a property"));
                    }
                }
            }
            for (k, child) in props {
                node(child, &format!("{path}/properties/{k}"), false)?;
            }
        }
        "array" => {
            if !obj.get("maxItems").is_some_and(Value::is_u64) {
                return Err(format!("{path}: arrays need maxItems"));
            }
            let items = obj
                .get("items")
                .ok_or_else(|| format!("{path}: arrays need items"))?;
            node(items, &format!("{path}/items"), false)?;
        }
        "string" => {
            if !obj.contains_key("enum") && !obj.get("maxLength").is_some_and(Value::is_u64) {
                return Err(format!("{path}: strings need maxLength or enum"));
            }
        }
        "number" | "integer" | "boolean" => {}
        other => return Err(format!("{path}: unsupported type {other:?}")),
    }
    Ok(())
}

/// Expands one `verbatim` pointer (with `*` for array elements) against the schema into
/// concrete JSON pointers; the leaf must be a string, number or integer.
pub(crate) fn expand_pointer(schema: &Value, pattern: &str) -> Result<Vec<String>, String> {
    let rest = pattern
        .strip_prefix('/')
        .ok_or_else(|| format!("verbatim pointer {pattern:?} must start with '/'"))?;
    let mut out = vec![String::new()];
    let mut cur = schema;
    for seg in rest.split('/') {
        let ty = cur.get("type").and_then(Value::as_str).unwrap_or_default();
        if seg == "*" {
            if ty != "array" {
                return Err(format!("verbatim pointer {pattern:?}: '*' on a non-array"));
            }
            let n = cur.get("maxItems").and_then(Value::as_u64).unwrap_or(0) as usize;
            if out.len().saturating_mul(n) > MAX_EXPANSION {
                return Err(format!("verbatim pointer {pattern:?} expands too far"));
            }
            out = out
                .iter()
                .flat_map(|p| (0..n).map(move |i| format!("{p}/{i}")))
                .collect();
            cur = cur.get("items").unwrap_or(&Value::Null);
        } else {
            if ty != "object" {
                return Err(format!(
                    "verbatim pointer {pattern:?}: {seg:?} on a non-object"
                ));
            }
            cur = cur
                .get("properties")
                .and_then(|p| p.get(seg))
                .ok_or_else(|| format!("verbatim pointer {pattern:?}: no property {seg:?}"))?;
            for p in &mut out {
                p.push('/');
                p.push_str(seg);
            }
        }
    }
    match cur.get("type").and_then(Value::as_str) {
        Some("string" | "number" | "integer") => Ok(out),
        _ => Err(format!(
            "verbatim pointer {pattern:?} must end on a string or number"
        )),
    }
}

/// Top-level property names (for series key placeholders).
pub(crate) fn top_level_property(schema: &Value, name: &str) -> bool {
    schema.get("properties").and_then(|p| p.get(name)).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn expands_nested_wildcards() {
        let s = json!({"type": "object", "properties": {
            "a": {"type": "array", "maxItems": 2, "items": {"type": "object", "properties": {
                "b": {"type": "array", "maxItems": 2, "items": {"type": "string"}}}}}}});
        assert_eq!(
            expand_pointer(&s, "/a/*/b/*").unwrap(),
            vec!["/a/0/b/0", "/a/0/b/1", "/a/1/b/0", "/a/1/b/1"]
        );
        assert!(expand_pointer(&s, "a").is_err());
        assert!(expand_pointer(&s, "/a/*").is_err(), "leaf is an object");
        assert!(expand_pointer(&s, "/a/b").is_err(), "missing '*'");
    }

    #[test]
    fn expansion_is_bounded() {
        let s = json!({"type": "object", "properties": {
            "a": {"type": "array", "maxItems": 100, "items": {"type": "object", "properties": {
                "b": {"type": "array", "maxItems": 100, "items": {"type": "string"}}}}}}});
        assert!(expand_pointer(&s, "/a/*/b/*").is_err());
    }

    #[test]
    fn nested_schema_keyword_rejected() {
        let s = json!({"$schema": DRAFT_2020_12, "type": "object", "additionalProperties": false,
            "properties": {"a": {"$schema": DRAFT_2020_12, "type": "string", "maxLength": 1}}});
        assert!(lint(&s).is_err());
    }
}
