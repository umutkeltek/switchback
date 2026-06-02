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

use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaLossiness {
    Metadata,
    Low,
    Medium,
    High,
}

impl SchemaLossiness {
    pub fn as_str(self) -> &'static str {
        match self {
            SchemaLossiness::Metadata => "metadata",
            SchemaLossiness::Low => "low",
            SchemaLossiness::Medium => "medium",
            SchemaLossiness::High => "high",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaWarning {
    pub path: String,
    pub keyword: String,
    pub lossiness: SchemaLossiness,
    pub message: String,
}

impl SchemaWarning {
    fn new(
        path: impl Into<String>,
        keyword: impl Into<String>,
        lossiness: SchemaLossiness,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            keyword: keyword.into(),
            lossiness,
            message: message.into(),
        }
    }

    pub fn prepend_path(mut self, prefix: &str) -> Self {
        if prefix.is_empty() {
            return self;
        }
        self.path = if self.path.is_empty() {
            prefix.to_string()
        } else {
            format!("{prefix}{}", self.path)
        };
        self
    }
}

impl std::fmt::Display for SchemaWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = if self.path.is_empty() {
            "/"
        } else {
            self.path.as_str()
        };
        write!(
            f,
            "schema_downlevel:{}:{}:{}: {}",
            self.lossiness.as_str(),
            path,
            self.keyword,
            self.message
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownlevelResult {
    pub schema: Value,
    pub warnings: Vec<SchemaWarning>,
}

/// Downlevel a JSON Schema to what `caps` accepts. Recursive into
/// `properties`/`items`; lossy where the target can't represent a construct.
pub fn downlevel(schema: &Value, caps: &SchemaCaps) -> Value {
    downlevel_with_warnings(schema, caps).schema
}

pub fn downlevel_with_warnings(schema: &Value, caps: &SchemaCaps) -> DownlevelResult {
    let mut warnings = Vec::new();
    let schema = downlevel_value(schema, caps, "", 0, &mut warnings);
    DownlevelResult { schema, warnings }
}

/// Hard cap on schema nesting depth. Real tool schemas are only a handful of
/// levels deep; without a bound, an adversarial deeply nested `parameters`
/// schema from an untrusted request would recurse until the stack overflows and
/// aborts the process. Past this depth we truncate to a permissive value.
const MAX_DOWNLEVEL_DEPTH: usize = 100;

fn downlevel_value(
    schema: &Value,
    caps: &SchemaCaps,
    path: &str,
    depth: usize,
    warnings: &mut Vec<SchemaWarning>,
) -> Value {
    match schema {
        Value::Object(obj) => Value::Object(downlevel_object(obj, caps, path, depth, warnings)),
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

fn escape_json_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn join_path(path: &str, segment: &str) -> String {
    let segment = escape_json_pointer(segment);
    if path.is_empty() {
        format!("/{segment}")
    } else {
        format!("{path}/{segment}")
    }
}

fn warn(
    warnings: &mut Vec<SchemaWarning>,
    path: &str,
    keyword: &str,
    lossiness: SchemaLossiness,
    message: impl Into<String>,
) {
    warnings.push(SchemaWarning::new(
        join_path(path, keyword),
        keyword,
        lossiness,
        message,
    ));
}

fn downlevel_object(
    obj: &Map<String, Value>,
    caps: &SchemaCaps,
    path: &str,
    depth: usize,
    warnings: &mut Vec<SchemaWarning>,
) -> Map<String, Value> {
    if depth >= MAX_DOWNLEVEL_DEPTH {
        warn(
            warnings,
            path,
            "$depth",
            SchemaLossiness::High,
            format!(
                "schema nesting exceeded the maximum supported depth ({MAX_DOWNLEVEL_DEPTH}); truncated to a permissive value"
            ),
        );
        let mut placeholder = Map::new();
        placeholder.insert("type".to_string(), Value::String("string".to_string()));
        return placeholder;
    }
    // Union collapse first — may replace the whole node with one branch.
    if !caps.unions {
        if let Some((keyword, branches)) = obj
            .get("anyOf")
            .and_then(Value::as_array)
            .map(|branches| ("anyOf", branches))
            .or_else(|| {
                obj.get("oneOf")
                    .and_then(Value::as_array)
                    .map(|branches| ("oneOf", branches))
            })
        {
            warn(
                warnings,
                path,
                keyword,
                SchemaLossiness::High,
                format!("collapsed unsupported `{keyword}` union to one branch"),
            );
            if let Some(branch) = pick_branch(branches) {
                let mut chosen = downlevel_object(branch, caps, path, depth + 1, warnings);
                if let Some(desc) = obj.get("description") {
                    chosen.entry("description").or_insert_with(|| desc.clone());
                }
                return chosen;
            }
        }
    }

    let mut out = Map::new();
    for (key, value) in obj {
        if ALWAYS_STRIP.contains(&key.as_str()) || key.starts_with("x-") {
            warn(
                warnings,
                path,
                key,
                SchemaLossiness::Metadata,
                "stripped schema metadata unsupported by provider dialect",
            );
            continue;
        }
        if !caps.unions && (key == "anyOf" || key == "oneOf") {
            warn(
                warnings,
                path,
                key,
                SchemaLossiness::High,
                "dropped unsupported union keyword",
            );
            continue;
        }
        if !caps.all_of && key == "allOf" {
            warn(
                warnings,
                path,
                key,
                SchemaLossiness::Medium,
                "merged unsupported `allOf` shallowly",
            );
            continue; // merged in below
        }
        if !caps.const_keyword && key == "const" {
            continue; // rewritten below
        }
        if !caps.refs && (key == "$ref" || key == "$defs" || key == "definitions") {
            warn(
                warnings,
                path,
                key,
                SchemaLossiness::High,
                "dropped unsupported JSON Schema reference keyword",
            );
            continue;
        }
        if !caps.additional_properties && key == "additionalProperties" {
            warn(
                warnings,
                path,
                key,
                SchemaLossiness::Medium,
                "dropped unsupported `additionalProperties` constraint",
            );
            continue;
        }

        match key.as_str() {
            "type" if !caps.type_arrays => {
                if let Value::Array(types) = value {
                    if types.len() > 1 {
                        warn(
                            warnings,
                            path,
                            "type",
                            SchemaLossiness::Medium,
                            "collapsed unsupported type array to one type",
                        );
                    }
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
                    if values.iter().any(|v| !v.is_string()) {
                        warn(
                            warnings,
                            path,
                            "enum",
                            SchemaLossiness::Low,
                            "stringified non-string enum values",
                        );
                    }
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
                        let prop_path = join_path(&join_path(path, "properties"), pk);
                        new_props.insert(
                            pk.clone(),
                            downlevel_value(pv, caps, &prop_path, depth + 1, warnings),
                        );
                    }
                    out.insert("properties".to_string(), Value::Object(new_props));
                }
            }
            "items" => match value {
                Value::Array(items) if !caps.type_arrays => {
                    warn(
                        warnings,
                        path,
                        "items",
                        SchemaLossiness::Medium,
                        "collapsed unsupported tuple item schemas to the first item",
                    );
                    if let Some(first) = items.first() {
                        out.insert(
                            "items".to_string(),
                            downlevel_value(
                                first,
                                caps,
                                &join_path(path, "items"),
                                depth + 1,
                                warnings,
                            ),
                        );
                    }
                }
                _ => {
                    out.insert(
                        "items".to_string(),
                        downlevel_value(
                            value,
                            caps,
                            &join_path(path, "items"),
                            depth + 1,
                            warnings,
                        ),
                    );
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
            warn(
                warnings,
                path,
                "const",
                SchemaLossiness::Low,
                "rewrote unsupported `const` as a single-value enum",
            );
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
            for (idx, member) in members.iter().enumerate() {
                if let Value::Object(member) = member {
                    let member_path = join_path(&join_path(path, "allOf"), &idx.to_string());
                    let down = downlevel_object(member, caps, &member_path, depth + 1, warnings);
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
            warn(
                warnings,
                path,
                "properties",
                SchemaLossiness::Medium,
                "added placeholder property to unsupported empty object schema",
            );
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
        let result = downlevel_with_warnings(&schema, &SchemaCaps::gemini());
        let out = result.schema;
        assert!(out.get("anyOf").is_none());
        assert_eq!(out["type"], "string"); // the non-null branch won
        assert_eq!(out["description"], "a value"); // parent description preserved
        assert!(result.warnings.iter().any(|warning| {
            warning.keyword == "anyOf" && warning.lossiness == SchemaLossiness::High
        }));
    }

    #[test]
    fn deeply_nested_schema_is_truncated_not_overflowed() {
        // A pathologically deep `properties` chain (reachable from an untrusted
        // tool `parameters` schema) must not recurse until the stack overflows.
        // Past MAX_DOWNLEVEL_DEPTH the node is truncated with a high-lossiness
        // `$depth` warning. 500 is 5x the cap — enough to prove truncation while
        // keeping the serde_json Value's own recursive Drop shallow.
        let mut schema = json!({ "type": "string" });
        for _ in 0..500 {
            schema = json!({ "type": "object", "properties": { "a": schema } });
        }
        let result = downlevel_with_warnings(&schema, &SchemaCaps::gemini());
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.keyword == "$depth" && w.lossiness == SchemaLossiness::High),
            "a schema deeper than the cap should emit a depth-truncation warning"
        );
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
        let result = downlevel_with_warnings(&schema, &SchemaCaps::gemini());
        let out = result.schema;
        assert_eq!(out["type"], "string");
        assert!(out.get("$ref").is_none());
        assert!(out.get("additionalProperties").is_none());
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.keyword == "$ref"
                    && warning.lossiness == SchemaLossiness::High)
        );
        assert!(result
            .warnings
            .iter()
            .any(|warning| warning.keyword == "additionalProperties"
                && warning.lossiness == SchemaLossiness::Medium));
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
