//! JSON-Schema downleveling (deconstruction §13.2 keep-list, audit §9.8).
//!
//! Providers accept different JSON-Schema dialects. Gemini's
//! `functionDeclarations` (and `responseSchema`) are a restricted OpenAPI-3.0
//! subset: no `anyOf`/`oneOf` unions, no `const`, no `type` arrays, string enums
//! only, no `$ref`, and object schemas must have properties. Rather than reject a
//! tool whose schema uses those, we **downlevel** the schema to what the target
//! accepts — the same hub-and-spoke philosophy applied to schemas.
//!
//! It is **capability-driven** (a [`SchemaCaps`]), not provider-name-hardcoded,
//! and **lossy by necessity** (`anyOf` -> best branch, `$ref` dropped) — that
//! loss is documented, and the alternative is a hard rejection.

use serde_json::{Map, Value};

/// Which JSON-Schema features a target's tool / structured-output dialect
/// accepts. A `false` triggers the corresponding downlevel transform.
#[derive(Debug, Clone, Copy)]
pub struct SchemaCaps {
    /// `anyOf` / `oneOf` unions (else collapsed to the best branch).
    pub unions: bool,
    /// `allOf` (else shallow-merged into the node).
    pub all_of: bool,
    /// `const` (else rewritten to a single-value `enum`).
    pub const_keyword: bool,
    /// `type: ["string","null"]` arrays (else first non-null type).
    pub type_arrays: bool,
    /// `$ref` / `$defs` (else dropped — lossy).
    pub refs: bool,
    /// Enum values must all be strings.
    pub string_enums_only: bool,
    /// Object schemas with no `properties` (else a placeholder prop is added).
    pub empty_objects: bool,
    /// `additionalProperties`.
    pub additional_properties: bool,
}

impl SchemaCaps {
    /// Full JSON Schema — downleveling is a near no-op (only universally-unsafe
    /// keys like `$schema` / `x-*` are stripped).
    pub fn permissive() -> Self {
        Self {
            unions: true,
            all_of: true,
            const_keyword: true,
            type_arrays: true,
            refs: true,
            string_enums_only: false,
            empty_objects: true,
            additional_properties: true,
        }
    }

    /// Gemini's restricted `functionDeclarations` / `responseSchema` dialect.
    pub fn gemini() -> Self {
        Self {
            unions: false,
            all_of: false,
            const_keyword: false,
            type_arrays: false,
            refs: false,
            string_enums_only: true,
            empty_objects: false,
            additional_properties: false,
        }
    }
}

/// Keys stripped regardless of caps — meta keywords no provider needs in a tool
/// parameter schema. Vendor `x-*` extensions are stripped separately by prefix.
const ALWAYS_STRIP: &[&str] = &["$schema", "$id", "$comment", "$anchor", "examples"];

/// Downlevel a JSON Schema to what `caps` accepts. Recursive into
/// `properties`/`items`; lossy where the target can't represent a construct.
pub fn downlevel(schema: &Value, caps: &SchemaCaps) -> Value {
    match schema {
        Value::Object(obj) => Value::Object(downlevel_object(obj, caps)),
        other => other.clone(),
    }
}

fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(), // numbers/bools -> "3" / "true"
    }
}

/// First non-`null`-typed branch of a union, else the first object branch.
fn pick_branch(branches: &[Value]) -> Option<&Map<String, Value>> {
    branches
        .iter()
        .find_map(|b| {
            let obj = b.as_object()?;
            if obj.get("type").and_then(Value::as_str) == Some("null") {
                None
            } else {
                Some(obj)
            }
        })
        .or_else(|| branches.iter().find_map(Value::as_object))
}

fn merge_into(out: &mut Map<String, Value>, src: &Map<String, Value>) {
    for (key, value) in src {
        match (key.as_str(), out.get_mut(key)) {
            ("properties", Some(Value::Object(existing))) => {
                if let Value::Object(incoming) = value {
                    for (pk, pv) in incoming {
                        existing.entry(pk.clone()).or_insert_with(|| pv.clone());
                    }
                }
            }
            ("required", Some(Value::Array(existing))) => {
                if let Value::Array(incoming) = value {
                    for item in incoming {
                        if !existing.contains(item) {
                            existing.push(item.clone());
                        }
                    }
                }
            }
            (_, None) => {
                out.insert(key.clone(), value.clone());
            }
            _ => {} // keep existing
        }
    }
}

fn downlevel_object(obj: &Map<String, Value>, caps: &SchemaCaps) -> Map<String, Value> {
    // Union collapse first — may replace the whole node with one branch.
    if !caps.unions {
        if let Some(branch) = obj
            .get("anyOf")
            .or_else(|| obj.get("oneOf"))
            .and_then(Value::as_array)
            .and_then(|b| pick_branch(b))
        {
            let mut chosen = downlevel_object(branch, caps);
            if let Some(desc) = obj.get("description") {
                chosen.entry("description").or_insert_with(|| desc.clone());
            }
            return chosen;
        }
    }

    let mut out = Map::new();
    for (key, value) in obj {
        if ALWAYS_STRIP.contains(&key.as_str()) || key.starts_with("x-") {
            continue;
        }
        if !caps.unions && (key == "anyOf" || key == "oneOf") {
            continue;
        }
        if !caps.all_of && key == "allOf" {
            continue; // merged in below
        }
        if !caps.const_keyword && key == "const" {
            continue; // rewritten below
        }
        if !caps.refs && (key == "$ref" || key == "$defs" || key == "definitions") {
            continue;
        }
        if !caps.additional_properties && key == "additionalProperties" {
            continue;
        }

        match key.as_str() {
            "type" if !caps.type_arrays => {
                if let Value::Array(types) = value {
                    let chosen = types
                        .iter()
                        .find(|t| t.as_str() != Some("null"))
                        .or_else(|| types.first());
                    if let Some(t) = chosen {
                        out.insert("type".to_string(), t.clone());
                    }
                } else {
                    out.insert("type".to_string(), value.clone());
                }
            }
            "enum" if caps.string_enums_only => {
                if let Value::Array(values) = value {
                    let strings: Vec<Value> =
                        values.iter().map(|v| Value::String(stringify(v))).collect();
                    out.insert("enum".to_string(), Value::Array(strings));
                    out.insert("type".to_string(), Value::String("string".to_string()));
                } else {
                    out.insert("enum".to_string(), value.clone());
                }
            }
            "properties" => {
                if let Value::Object(props) = value {
                    let mut new_props = Map::new();
                    for (pk, pv) in props {
                        new_props.insert(pk.clone(), downlevel(pv, caps));
                    }
                    out.insert("properties".to_string(), Value::Object(new_props));
                }
            }
            "items" => match value {
                Value::Array(items) if !caps.type_arrays => {
                    if let Some(first) = items.first() {
                        out.insert("items".to_string(), downlevel(first, caps));
                    }
                }
                _ => {
                    out.insert("items".to_string(), downlevel(value, caps));
                }
            },
            _ => {
                out.insert(key.clone(), value.clone());
            }
        }
    }

    // const -> single-value enum.
    if !caps.const_keyword {
        if let Some(value) = obj.get("const") {
            let entry = if caps.string_enums_only {
                out.insert("type".to_string(), Value::String("string".to_string()));
                Value::String(stringify(value))
            } else {
                value.clone()
            };
            out.insert("enum".to_string(), Value::Array(vec![entry]));
        }
    }

    // allOf -> shallow merge member schemas.
    if !caps.all_of {
        if let Some(Value::Array(members)) = obj.get("allOf") {
            for member in members {
                if let Value::Object(member) = member {
                    let down = downlevel_object(member, caps);
                    merge_into(&mut out, &down);
                }
            }
        }
    }

    // Object with no properties -> inject a placeholder (Gemini rejects them).
    if !caps.empty_objects && out.get("type").and_then(Value::as_str) == Some("object") {
        let empty = out
            .get("properties")
            .and_then(Value::as_object)
            .map(Map::is_empty)
            .unwrap_or(true);
        if empty {
            let mut placeholder = Map::new();
            placeholder.insert("type".to_string(), Value::String("string".to_string()));
            placeholder.insert(
                "description".to_string(),
                Value::String("placeholder (schema declared no properties)".to_string()),
            );
            let mut props = Map::new();
            props.insert("_".to_string(), Value::Object(placeholder));
            out.insert("properties".to_string(), Value::Object(props));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permissive_strips_only_meta_keys() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "x-vendor": "gizmo",
            "type": "object",
            "anyOf": [{ "type": "string" }],
            "const": 5,
            "properties": { "a": { "type": "string" } }
        });
        let out = downlevel(&schema, &SchemaCaps::permissive());
        // meta + vendor stripped...
        assert!(out.get("$schema").is_none());
        assert!(out.get("x-vendor").is_none());
        // ...but anyOf/const preserved under permissive caps.
        assert!(out.get("anyOf").is_some());
        assert_eq!(out["const"], 5);
    }

    #[test]
    fn gemini_collapses_union_to_best_branch() {
        let schema = json!({
            "description": "a value",
            "anyOf": [ { "type": "null" }, { "type": "string", "minLength": 1 } ]
        });
        let out = downlevel(&schema, &SchemaCaps::gemini());
        assert!(out.get("anyOf").is_none());
        assert_eq!(out["type"], "string"); // the non-null branch won
        assert_eq!(out["description"], "a value"); // parent description preserved
    }

    #[test]
    fn gemini_rewrites_const_and_stringifies_enums() {
        let schema = json!({ "const": 7 });
        let out = downlevel(&schema, &SchemaCaps::gemini());
        assert!(out.get("const").is_none());
        assert_eq!(out["enum"], json!(["7"])); // const -> string enum
        assert_eq!(out["type"], "string");

        let enum_schema = json!({ "enum": [1, 2, 3] });
        let out = downlevel(&enum_schema, &SchemaCaps::gemini());
        assert_eq!(out["enum"], json!(["1", "2", "3"]));
        assert_eq!(out["type"], "string");
    }

    #[test]
    fn gemini_flattens_type_arrays_and_strips_refs() {
        let schema = json!({
            "type": ["string", "null"],
            "$ref": "#/$defs/Thing",
            "additionalProperties": false
        });
        let out = downlevel(&schema, &SchemaCaps::gemini());
        assert_eq!(out["type"], "string");
        assert!(out.get("$ref").is_none());
        assert!(out.get("additionalProperties").is_none());
    }

    #[test]
    fn gemini_adds_placeholder_to_empty_object_and_recurses() {
        let schema = json!({
            "type": "object",
            "properties": {
                "nested": { "type": "object" }, // empty -> placeholder
                "tag": { "const": "x" }          // nested const -> enum
            }
        });
        let out = downlevel(&schema, &SchemaCaps::gemini());
        assert!(out["properties"]["nested"]["properties"]["_"].is_object());
        assert_eq!(out["properties"]["tag"]["enum"], json!(["x"]));
    }
}
