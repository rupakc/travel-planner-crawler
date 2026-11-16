//! JSON Schema (the kinds' `schemas/<kind>.v1.json`) → Vertex `responseSchema` (the OpenAPI 3.0
//! subset: upper-case `type`, `nullable`, `enum`, `required`, `properties`, `items`, `anyOf`).
//!
//! Local `$ref`s (`#/$defs/…`, `#/definitions/…`) are inlined. Everything outside an allow-list
//! is dropped: `$schema`, `$id`, `$defs`, `additionalProperties`, `pattern` (regexes are
//! rejected or ignored by constrained decoding), `examples`, `if/then/else`, `not`, … Full JSON
//! Schema validation of the answer stays in vc-extract.

use serde_json::{Map, Value};

const KEEP: &[&str] = &[
    "description",
    "format",
    "title",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "minProperties",
    "maxProperties",
    "example",
    "default",
    "propertyOrdering",
];

const MAX_DEPTH: usize = 32;

/// Converts a JSON Schema to a Vertex `responseSchema`.
pub fn to_vertex_schema(schema: &Value) -> Value {
    convert(schema, schema, 0)
}

fn resolve<'a>(root: &'a Value, r: &str) -> Option<&'a Value> {
    let ptr = r.strip_prefix('#')?;
    root.pointer(ptr)
}

fn upper(t: &str) -> Value {
    Value::String(t.to_ascii_uppercase())
}

fn convert(node: &Value, root: &Value, depth: usize) -> Value {
    let Some(obj) = node.as_object() else {
        return Value::Object(Map::new());
    };
    if depth > MAX_DEPTH {
        return Value::Object(Map::new());
    }
    if let Some(r) = obj.get("$ref").and_then(Value::as_str)
        && let Some(target) = resolve(root, r)
    {
        let mut out = convert(target, root, depth + 1);
        // Sibling keywords (e.g. a description next to the $ref) win.
        let mut rest = obj.clone();
        rest.remove("$ref");
        if let (Some(o), Value::Object(extra)) = (
            out.as_object_mut(),
            convert(&Value::Object(rest), root, depth + 1),
        ) {
            for (k, v) in extra {
                o.insert(k, v);
            }
        }
        return out;
    }

    let mut out = Map::new();
    let mut nullable = obj
        .get("nullable")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    match obj.get("type") {
        Some(Value::String(t)) if t == "null" => {
            out.insert("type".into(), upper("null"));
        }
        Some(Value::String(t)) => {
            out.insert("type".into(), upper(t));
        }
        Some(Value::Array(ts)) => {
            let non_null: Vec<&str> = ts
                .iter()
                .filter_map(Value::as_str)
                .filter(|t| *t != "null")
                .collect();
            if non_null.len() < ts.len() {
                nullable = true;
            }
            match non_null.as_slice() {
                [] => {}
                [one] => {
                    out.insert("type".into(), upper(one));
                }
                many => {
                    out.insert(
                        "anyOf".into(),
                        Value::Array(
                            many.iter()
                                .map(|t| {
                                    let mut m = Map::new();
                                    m.insert("type".into(), upper(t));
                                    Value::Object(m)
                                })
                                .collect(),
                        ),
                    );
                }
            }
        }
        _ => {
            if obj.contains_key("properties") {
                out.insert("type".into(), upper("object"));
            } else if obj.contains_key("items") {
                out.insert("type".into(), upper("array"));
            }
        }
    }

    for k in KEEP {
        if let Some(v) = obj.get(*k) {
            out.insert((*k).to_string(), v.clone());
        }
    }
    if let Some(v) = obj.get("required").filter(|v| v.is_array()) {
        out.insert("required".into(), v.clone());
    }

    let mut en: Option<Vec<Value>> = obj.get("enum").and_then(Value::as_array).cloned();
    if let Some(c) = obj.get("const") {
        en = Some(vec![c.clone()]);
    }
    if let Some(mut vals) = en {
        if vals.iter().any(Value::is_null) {
            nullable = true;
            vals.retain(|v| !v.is_null());
        }
        if !out.contains_key("type") && vals.iter().all(Value::is_string) {
            out.insert("type".into(), upper("string"));
        }
        out.insert("enum".into(), Value::Array(vals));
    }

    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        let mut p = Map::new();
        for (k, v) in props {
            p.insert(k.clone(), convert(v, root, depth + 1));
        }
        out.insert("properties".into(), Value::Object(p));
    }
    if let Some(items) = obj.get("items") {
        out.insert("items".into(), convert(items, root, depth + 1));
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(alts) = obj.get(key).and_then(Value::as_array) {
            let mut list: Vec<Value> = Vec::new();
            for a in alts {
                if a.get("type").and_then(Value::as_str) == Some("null") {
                    nullable = true;
                } else {
                    list.push(convert(a, root, depth + 1));
                }
            }
            if list.len() == 1 && !out.contains_key("type") {
                if let Some(Value::Object(one)) = list.pop() {
                    for (k, v) in one {
                        out.entry(k).or_insert(v);
                    }
                }
            } else if !list.is_empty() {
                out.insert("anyOf".into(), Value::Array(list));
            }
        }
    }
    if nullable {
        out.insert("nullable".into(), Value::Bool(true));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_types_nullable_refs_and_drops_unknown() {
        let s = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "x",
            "type": "object",
            "additionalProperties": false,
            "required": ["name"],
            "properties": {
                "name": {"type": "string", "description": "n"},
                "phone": {"type": ["string", "null"]},
                "kind": {"enum": ["a", "b", null]},
                "fixed": {"const": "v"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "price": {"$ref": "#/$defs/price"},
                "maybe": {"anyOf": [{"type": "integer"}, {"type": "null"}]}
            },
            "$defs": {"price": {"type": "object", "properties": {"amount": {"type": "number"}}}}
        });
        let v = to_vertex_schema(&s);
        assert_eq!(
            v,
            json!({
                "type": "OBJECT",
                "required": ["name"],
                "properties": {
                    "name": {"type": "STRING", "description": "n"},
                    "phone": {"type": "STRING", "nullable": true},
                    "kind": {"type": "STRING", "enum": ["a", "b"], "nullable": true},
                    "fixed": {"type": "STRING", "enum": ["v"]},
                    "tags": {"type": "ARRAY", "items": {"type": "STRING"}},
                    "price": {"type": "OBJECT", "properties": {"amount": {"type": "NUMBER"}}},
                    "maybe": {"type": "INTEGER", "nullable": true}
                }
            })
        );
    }

    #[test]
    fn recursive_ref_terminates() {
        let s = json!({"$defs": {"n": {"type": "object", "properties": {"next": {"$ref": "#/$defs/n"}}}},
                       "$ref": "#/$defs/n"});
        let v = to_vertex_schema(&s);
        assert_eq!(v["type"], "OBJECT");
    }
}
