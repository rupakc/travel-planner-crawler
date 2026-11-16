//! Every kind schema in `schemas/*.v<N>.json` converts to a Vertex `responseSchema` that uses
//! only the OpenAPI-subset keywords Vertex accepts.

use std::path::PathBuf;

use serde_json::Value;
use vc_llm::to_vertex_schema;

const ALLOWED: &[&str] = &[
    "type",
    "nullable",
    "enum",
    "required",
    "properties",
    "items",
    "anyOf",
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
const TYPES: &[&str] = &[
    "STRING", "NUMBER", "INTEGER", "BOOLEAN", "ARRAY", "OBJECT", "NULL",
];

/// Walks a converted schema node; `properties` maps names to schemas.
fn check(node: &Value, at: &str) {
    let obj = node
        .as_object()
        .unwrap_or_else(|| panic!("{at}: not an object"));
    for (k, v) in obj {
        assert!(
            ALLOWED.contains(&k.as_str()),
            "{at}: keyword {k} not allowed"
        );
        match k.as_str() {
            "type" => assert!(
                v.as_str().is_some_and(|t| TYPES.contains(&t)),
                "{at}: bad type {v}"
            ),
            "properties" => {
                for (name, sub) in v.as_object().expect("properties object") {
                    check(sub, &format!("{at}.{name}"));
                }
            }
            "items" => check(v, &format!("{at}[]")),
            "anyOf" => {
                for (i, sub) in v.as_array().expect("anyOf array").iter().enumerate() {
                    check(sub, &format!("{at}|{i}"));
                }
            }
            "enum" => assert!(
                v.as_array().is_some_and(|a| a.iter().all(|x| !x.is_null())),
                "{at}: null in enum"
            ),
            _ => {}
        }
    }
}

#[test]
fn every_kind_schema_converts_to_vertex_subset() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas");
    let mut n = 0;
    for entry in std::fs::read_dir(&dir).expect("schemas/ exists") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // Kind schemas only (`<kind>.v<N>.json`), not source.schema.json.
        if !name.ends_with(".json") || !name.contains(".v") || name.contains(".schema.") {
            continue;
        }
        let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let v = to_vertex_schema(&schema);
        check(&v, &name);
        assert_eq!(v["type"], "OBJECT", "{name}: top level is an object");
        let text = v.to_string();
        for banned in [
            "$schema",
            "$id",
            "$ref",
            "$defs",
            "additionalProperties",
            "pattern",
        ] {
            assert!(
                !text.contains(&format!("\"{banned}\"")),
                "{name}: {banned} left"
            );
        }
        n += 1;
    }
    assert!(n > 0, "no kind schemas found in {}", dir.display());
}
