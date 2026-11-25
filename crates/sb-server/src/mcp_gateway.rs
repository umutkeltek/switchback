use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderValue, AUTHORIZATION, IF_MATCH};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::handlers::setup::SCHEMA;

const SETUP_APPLY: &str = "switchback_setup_apply";
const SETTINGS_REVERT: &str = "switchback_settings_revert";
const NAMES_REVERT: &str = "switchback_names_revert";
const KEY_ENV: &str = "SWITCHBACK_AI_KEY";

pub(crate) struct GatewayTools {
    base: Url,
    client: reqwest::Client,
    key: Option<String>,
    authorization: Option<HeaderValue>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Target {
    target: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SettingsGet {
    target: String,
    profile: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SettingsSet {
    target: String,
    revision: u64,
    patch: Map<String, Value>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SettingsRevert {
    target: String,
    revision: u64,
    to: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Name {
    name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct NamesSet {
    name: String,
    revision: u64,
    targets: Vec<String>,
    pin_order: Option<bool>,
    prefer_awake: Option<sb_core::PreferAwakeOverride>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct NamesRevert {
    name: String,
    revision: u64,
    to: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetupExport {
    #[serde(default)]
    effective: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetupPlan {
    document: Map<String, Value>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetupApply {
    document: Map<String, Value>,
    #[serde(default)]
    confirm: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct RoutePreview {
    model: String,
    #[serde(default)]
    stream: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

fn tool<T: schemars::JsonSchema>(name: &str, description: &str, writes: bool) -> Value {
    let mut schema = json!(schemars::schema_for!(T));
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
        "annotations": {"readOnlyHint": !writes, "openWorldHint": true}
    })
}

pub(crate) fn definitions() -> Vec<Value> {
    let mut apply = tool::<SetupApply>(
        SETUP_APPLY,
        "Apply a setup document with confirm=true; changed items include an undo, and the top-level undo call restores them if their revisions still match.",
        true,
    );
    apply["inputSchema"]["required"] = json!(["document", "confirm"]);
    apply["inputSchema"]["properties"]["confirm"] = json!({"const": true});
    vec![
        tool::<SetupExport>("switchback_setup_export", "Read the saved setup document; effective=true also includes names defined by gateway config.", false),
        tool::<NoArgs>("switchback_setup_schema", "Read the JSON Schema for a setup document.", false),
        tool::<SetupPlan>("switchback_setup_plan", "Show planned changes and current revisions for a setup document without writing; inspect every item before applying.", false),
        apply,
        tool::<SettingsGet>("switchback_settings_get", "Read effective settings and saved documents for a model or target, optionally for a profile.", false),
        tool::<Target>("switchback_settings_history", "Read the saved settings revisions for one provider/model target.", false),
        tool::<SettingsSet>("switchback_settings_set", "Merge patch one target's stored settings using its current revision; null removes a field. Undo with the returned tool call.", true),
        tool::<SettingsRevert>(SETTINGS_REVERT, "Restore a target's earlier settings revision (to=0 clears overrides). Undo with the returned tool call.", true),
        tool::<NoArgs>("switchback_names_list", "List the public names and their current routing targets.", false),
        tool::<Name>("switchback_names_get", "Read a public name's targets, current revision, and history.", false),
        tool::<NamesSet>("switchback_names_set", "Replace the ordered targets for a public name using its current revision (0 for a new name); undo with the returned names_revert call.", true),
        tool::<NamesRevert>(NAMES_REVERT, "Restore a public name's earlier revision (to=0 hands it back to config); undo with the returned names_revert call.", true),
        tool::<NoArgs>("switchback_models_list", "List models and route names offered by the gateway.", false),
        tool::<NoArgs>("switchback_engines_list", "List managed engines, their readiness, and their known models.", false),
        tool::<RoutePreview>("switchback_route_preview", "Preview the live route for a model without making an inference request.", false),
        tool::<NoArgs>("switchback_status", "Read gateway health, provider readiness, and admission status.", false),
    ]
}

impl GatewayTools {
    pub(crate) fn new(base: &str) -> Result<Self> {
        let base = Url::parse(base).context("gateway must be an HTTP or HTTPS base URL")?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            bail!(
                "gateway must be an HTTP or HTTPS base URL without credentials, query, or fragment"
            );
        }
        let key = match std::env::var(KEY_ENV) {
            Ok(key) if !key.is_empty() => Some(key),
            Ok(_) | Err(std::env::VarError::NotPresent) => None,
            Err(_) => bail!("{KEY_ENV} must contain a valid bearer key"),
        };
        let authorization = key
            .as_ref()
            .map(|key| {
                let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
                    .context("SWITCHBACK_AI_KEY must contain a valid bearer key")?;
                value.set_sensitive(true);
                Ok::<_, anyhow::Error>(value)
            })
            .transpose()?;
        let client = reqwest::Client::builder()
            .user_agent("switchback-mcp")
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            base,
            client,
            key,
            authorization,
        })
    }

    fn url(&self, path: &[&str]) -> Url {
        let mut url = self.base.clone();
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty().extend(path);
        }
        url
    }

    async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        revision: Option<u64>,
    ) -> Result<Value> {
        let mut request = self.client.request(method, url);
        if let Some(authorization) = &self.authorization {
            request = request.header(AUTHORIZATION, authorization);
        }
        if let Some(revision) = revision {
            request = request.header(IF_MATCH, revision.to_string());
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| {
            anyhow::anyhow!("Gateway request failed; read current state before retrying a write.")
        })?;
        let status = response.status();
        let result = response.json::<Value>().await;
        if !status.is_success() {
            let mut value = result.ok().filter(|value| value["error"].is_object()).unwrap_or_else(|| {
                json!({"error": {"message": "Gateway rejected the request.", "type": "gateway_error"}})
            });
            value["status"] = json!(status.as_u16());
            return Ok(value);
        }
        result.map_err(|_| anyhow::anyhow!("Gateway returned an invalid JSON response; read current state before retrying a write."))
    }

    async fn get(&self, path: &[&str]) -> Result<Value> {
        self.send(Method::GET, self.url(path), None, None).await
    }

    async fn setup(&self, action: &str, document: Map<String, Value>) -> Result<Value> {
        if document.contains_key("setup") {
            bail!("Pass the setup document directly, without a setup wrapper.");
        }
        for field in ["check", "apply_now", "wake"] {
            if document
                .get(field)
                .is_some_and(|value| value != &json!(false))
            {
                bail!("`document.{field}` must be false or omitted for MCP setup calls.");
            }
        }
        self.send(
            Method::POST,
            self.url(&["v1", "setup", action]),
            Some(json!(document)),
            None,
        )
        .await
    }

    async fn settings_write(
        &self,
        target: &str,
        revision: u64,
        method: Method,
        suffix: &[&str],
        body: Value,
    ) -> Result<Value> {
        validate_id(target)?;
        let mut path = vec!["v1", "models", target, "settings"];
        path.extend_from_slice(suffix);
        let mut result = self
            .send(method, self.url(&path), Some(body), Some(revision))
            .await?;
        if let Some(document) = result.get_mut("undo") {
            if document.is_object() {
                *document = undo(SETUP_APPLY, json!({"confirm": true, "document": document}));
            }
        } else if result.get("error").is_none() {
            result["undo"] = if result["unchanged"] == false {
                undo(
                    SETTINGS_REVERT,
                    json!({
                        "target": result["target"],
                        "revision": result["revision"],
                        "to": revision,
                    }),
                )
            } else {
                Value::Null
            };
        }
        Ok(result)
    }

    async fn names_write(
        &self,
        name: &str,
        revision: u64,
        method: Method,
        suffix: &[&str],
        body: Value,
    ) -> Result<Value> {
        validate_id(name)?;
        let mut path = vec!["v1", "roles", name];
        path.extend_from_slice(suffix);
        let mut result = self
            .send(method, self.url(&path), Some(body), Some(revision))
            .await?;
        if result.get("error").is_none() {
            result["undo"] = if result["unchanged"] == false {
                undo(
                    NAMES_REVERT,
                    json!({
                        "name": result["name"],
                        "revision": result["revision"],
                        "to": revision,
                    }),
                )
            } else {
                Value::Null
            };
        }
        Ok(result)
    }

    pub(crate) async fn call(&self, name: &str, args: Value) -> Value {
        let mut result = match self.call_inner(name, args).await {
            Ok(value) => value,
            Err(error) => json!({"error": {"type": "tool_error", "message": error.to_string()}}),
        };
        if let Some(key) = &self.key {
            redact(&mut result, key);
        }
        result
    }

    async fn call_inner(&self, name: &str, args: Value) -> Result<Value> {
        match name {
            "switchback_setup_export" => {
                let args: SetupExport = serde_json::from_value(args)?;
                let mut url = self.url(&["v1", "setup"]);
                if args.effective {
                    url.query_pairs_mut().append_pair("effective", "true");
                }
                self.send(Method::GET, url, None, None).await
            }
            "switchback_setup_plan" => {
                let args: SetupPlan = serde_json::from_value(args)?;
                self.setup("plan", args.document).await
            }
            SETUP_APPLY => {
                let args: SetupApply = serde_json::from_value(args)?;
                if !args.confirm {
                    bail!("Run switchback_setup_plan, review its items, then pass confirm: true to apply.");
                }
                let mut result = self.setup("apply", args.document).await?;
                if result.get("error").is_none() {
                    result["undo"] = setup_undo(&result);
                }
                Ok(result)
            }
            "switchback_settings_get" => {
                let args: SettingsGet = serde_json::from_value(args)?;
                validate_id(&args.target)?;
                let mut url = self.url(&["v1", "models", &args.target, "settings"]);
                if let Some(profile) = args.profile {
                    url.query_pairs_mut().append_pair("profile", &profile);
                }
                self.send(Method::GET, url, None, None).await
            }
            "switchback_settings_history" => {
                let args: Target = serde_json::from_value(args)?;
                validate_id(&args.target)?;
                self.get(&["v1", "models", &args.target, "settings", "history"])
                    .await
            }
            "switchback_settings_set" => {
                let args: SettingsSet = serde_json::from_value(args)?;
                for field in ["revision", "apply", "wake", "note"] {
                    if args.patch.contains_key(field) {
                        bail!("`patch.{field}` is not part of the settings document.");
                    }
                }
                self.settings_write(
                    &args.target,
                    args.revision,
                    Method::PATCH,
                    &[],
                    json!(args.patch),
                )
                .await
            }
            SETTINGS_REVERT => {
                let args: SettingsRevert = serde_json::from_value(args)?;
                self.settings_write(
                    &args.target,
                    args.revision,
                    Method::POST,
                    &["revert"],
                    json!({"revision": args.to}),
                )
                .await
            }
            "switchback_names_get" => {
                let args: Name = serde_json::from_value(args)?;
                validate_id(&args.name)?;
                self.get(&["v1", "roles", &args.name]).await
            }
            "switchback_names_set" => {
                let args: NamesSet = serde_json::from_value(args)?;
                let body = json!({"targets": args.targets, "pin_order": args.pin_order, "prefer_awake": args.prefer_awake});
                self.names_write(&args.name, args.revision, Method::PUT, &[], body)
                    .await
            }
            NAMES_REVERT => {
                let args: NamesRevert = serde_json::from_value(args)?;
                self.names_write(
                    &args.name,
                    args.revision,
                    Method::POST,
                    &["revert"],
                    json!({"to": args.to}),
                )
                .await
            }
            "switchback_route_preview" => {
                let args: RoutePreview = serde_json::from_value(args)?;
                let url = self.url(&["cp", "v1", "route-preview"]);
                let body = json!({"model": args.model, "stream": args.stream, "messages": [
                    {"role": "user", "content": "Route preview"}
                ]});
                self.send(Method::POST, url, Some(body), None).await
            }
            other => {
                let _: NoArgs = serde_json::from_value(args)?;
                let path = match other {
                    "switchback_setup_schema" => &["v1", "setup", "schema"][..],
                    "switchback_names_list" => &["v1", "roles"],
                    "switchback_models_list" => &["v1", "models"],
                    "switchback_engines_list" => &["v1", "engines"],
                    "switchback_status" => &["v1", "health"],
                    _ => bail!("Unknown gateway tool."),
                };
                self.get(path).await
            }
        }
    }
}

fn validate_id(id: &str) -> Result<()> {
    if matches!(id, "" | "." | "..") {
        bail!("Pass a non-empty model target or public name.");
    }
    Ok(())
}

fn undo(tool: &str, arguments: Value) -> Value {
    json!({"tool": tool, "arguments": arguments})
}

fn setup_undo(result: &Value) -> Value {
    let mut names = Map::new();
    let mut models = Map::new();
    let mut name_revisions = Map::new();
    let mut model_revisions = Map::new();
    let mut unlock = BTreeSet::new();
    for item in result["items"].as_array().into_iter().flatten() {
        let Some(item_undo) = item["undo"].as_object() else {
            continue;
        };
        for (family, docs, revisions) in [
            ("names", &mut names, &mut name_revisions),
            ("models", &mut models, &mut model_revisions),
        ] {
            if let Some(items) = item_undo.get(family).and_then(Value::as_object) {
                docs.extend(items.clone());
            }
            if let Some(items) = item_undo
                .get("if_match")
                .and_then(|value| value[family].as_object())
            {
                revisions.extend(items.clone());
            }
        }
        unlock.extend(
            item_undo
                .get("unlock")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        );
    }
    if names.is_empty() && models.is_empty() {
        return Value::Null;
    }
    undo(
        SETUP_APPLY,
        json!({"confirm": true, "document": {
            "schema": SCHEMA,
            "names": names,
            "models": models,
            "if_match": {"names": name_revisions, "models": model_revisions},
            "unlock": unlock,
        }}),
    )
}

fn redact(value: &mut Value, key: &str) {
    match value {
        Value::String(text) if text.contains(key) => *text = text.replace(key, "[redacted]"),
        Value::Array(items) => items.iter_mut().for_each(|item| redact(item, key)),
        Value::Object(object) => {
            let entries = std::mem::take(object);
            object.extend(entries.into_iter().map(|(name, mut value)| {
                redact(&mut value, key);
                (name.replace(key, "[redacted]"), value)
            }));
        }
        _ => {}
    }
}
