//! The read-only capability matrix: route (lane) -> target -> what the router
//! believes that target can do, plus an optional drift check against a model
//! registry built from live provider metadata.
//!
//! This exists because the capability flags the router hard-filters on were,
//! until now, invisible. An OpenAI-compatible provider defaults to
//! `vision_in: false` (deliberately conservative — an arbitrary OpenAI-shaped
//! endpoint may be text-only), and nothing surfaced that default, so a lane
//! whose model genuinely accepts images still rejected them. The failure was
//! only observable as a 400 at the coding harness, naming a capability nobody
//! had ever seen declared.
//!
//! Read-only by construction: no request is executed and no upstream is
//! contacted. `--check-drift` compares DECLARED flags with a registry file on
//! disk; it never mutates the registry or the config.

use std::collections::BTreeMap;
use std::path::Path;

use sb_core::{CapabilityProfile, Config};
use serde::{Deserialize, Serialize};

/// One target's resolved capability row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CapabilityRow {
    pub(crate) target_id: String,
    pub(crate) provider_id: String,
    /// False when the target id names a provider/model the config cannot
    /// resolve — a dead route entry, which is itself worth seeing.
    pub(crate) resolved: bool,
    pub(crate) vision_in: bool,
    /// Empty means "all known image source kinds" when `vision_in` is true.
    pub(crate) vision_sources: Vec<String>,
    pub(crate) tool_calling: bool,
    pub(crate) server_tools: bool,
    pub(crate) json_schema: bool,
    pub(crate) streaming: bool,
    pub(crate) max_context_tokens: Option<u32>,
    /// Where the flags came from, so an operator can tell a real declaration
    /// from an api-kind default they never chose.
    pub(crate) capability_source: String,
    /// Capabilities the provider's `capabilities:` block asserted that the
    /// RESOLVED profile does not agree with — i.e. the operator wrote a
    /// declaration that had no effect.
    ///
    /// This is the trap that makes a corrected flag look broken: a `catalog:`
    /// model row wins over a provider override (`target_for_provider_model`
    /// prefers the catalog entry), so declaring `vision_in: true` on the
    /// provider while a catalog row for that model says text-only silently
    /// changes nothing. Without this column the operator edits the config,
    /// restarts, sees the same 400, and concludes the fix does not work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) ignored_overrides: Vec<String>,
    /// Populated by `--check-drift`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) drift: Vec<String>,
}

/// One route (lane) and every target behind it, in declared fallback order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CapabilityLane {
    pub(crate) route: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) inbound_model: Option<String>,
    /// True when NO target on this lane accepts image input. With
    /// `server.vision_degrade` on (the default) an image request here is served
    /// as text; with it off the request 400s.
    pub(crate) vision_blind: bool,
    pub(crate) targets: Vec<CapabilityRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CapabilityMatrixSummary {
    pub(crate) lanes: Vec<CapabilityLane>,
    pub(crate) lane_count: usize,
    /// Lanes where an image request cannot be served natively by any target.
    pub(crate) vision_blind_lanes: Vec<String>,
    /// Effective `server.vision_degrade` — whether those lanes degrade or 400.
    pub(crate) vision_degrade: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) drift_source: Option<String>,
    pub(crate) drift_count: usize,
    /// Declarations that had no effect. Non-zero means a config edit silently
    /// did nothing.
    pub(crate) ignored_override_count: usize,
}

/// Every field the operator asserted that the resolved profile contradicts.
fn ignored_overrides(
    declared: &sb_core::CapabilityOverrides,
    resolved: &CapabilityProfile,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |name: &str, want: Option<bool>, got: bool| {
        if let Some(want) = want {
            if want != got {
                out.push(format!("{name}: declared {want} but resolves to {got}"));
            }
        }
    };
    check("vision_in", declared.vision_in, resolved.vision_in);
    check("tool_calling", declared.tool_calling, resolved.tool_calling);
    check("json_schema", declared.json_schema, resolved.json_schema);
    check("streaming", declared.streaming, resolved.streaming);
    check("server_tools", declared.server_tools, resolved.server_tools);
    check("audio_in", declared.audio_in, resolved.audio_in);
    check("file_in", declared.file_in, resolved.file_in);
    check("image_out", declared.image_out, resolved.image_out);
    if !declared.vision_sources.is_empty() && declared.vision_sources != resolved.vision_sources {
        out.push(format!(
            "vision_sources: declared {:?} but resolves to {:?}",
            declared
                .vision_sources
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
            resolved
                .vision_sources
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
        ));
    }
    out
}

fn sources_of(caps: &CapabilityProfile) -> Vec<String> {
    caps.vision_sources
        .iter()
        .map(|s| s.as_str().to_string())
        .collect()
}

/// Build the matrix from config alone. `registry_path`, when given, is a model
/// registry JSON used to flag drift between declared and observed capabilities.
pub(crate) fn capability_matrix(
    cfg: &Config,
    registry_path: Option<&Path>,
) -> anyhow::Result<CapabilityMatrixSummary> {
    let registry = sb_adapters::AdapterRegistry::from_config(cfg)
        .map_err(|e| anyhow::anyhow!("cannot build adapter registry: {e}"))?;

    // A provider carrying an explicit `capabilities:` block asserted its shape;
    // anything else is inheriting an api-kind default nobody chose. Naming the
    // difference is the point of the matrix.
    let declared: BTreeMap<&str, &sb_core::CapabilityOverrides> = cfg
        .providers
        .iter()
        .map(|p| (p.id.as_str(), &p.capabilities))
        .collect();

    let observed = match registry_path {
        Some(path) => Some(load_model_registry(path)?),
        None => None,
    };

    let mut lanes = Vec::new();
    let mut vision_blind_lanes = Vec::new();

    for route in &cfg.routes {
        let mut rows = Vec::new();
        for target_id in &route.targets {
            let resolved = registry.target_for(target_id);
            let mut row = match &resolved {
                Some(target) => CapabilityRow {
                    target_id: target_id.clone(),
                    provider_id: target.provider_id.clone(),
                    resolved: true,
                    vision_in: target.capabilities.vision_in,
                    vision_sources: sources_of(&target.capabilities),
                    tool_calling: target.capabilities.tool_calling,
                    server_tools: target.capabilities.server_tools,
                    json_schema: target.capabilities.json_schema,
                    streaming: target.capabilities.streaming,
                    max_context_tokens: target.capabilities.max_context_tokens,
                    capability_source: match declared.get(target.provider_id.as_str()) {
                        Some(o) if !o.is_empty() => "provider_override".to_string(),
                        _ => "api_kind_default".to_string(),
                    },
                    ignored_overrides: declared
                        .get(target.provider_id.as_str())
                        .map(|o| ignored_overrides(o, &target.capabilities))
                        .unwrap_or_default(),
                    drift: Vec::new(),
                },
                None => CapabilityRow {
                    target_id: target_id.clone(),
                    provider_id: target_id
                        .split_once('/')
                        .map(|(p, _)| p.to_string())
                        .unwrap_or_default(),
                    resolved: false,
                    vision_in: false,
                    vision_sources: Vec::new(),
                    tool_calling: false,
                    server_tools: false,
                    json_schema: false,
                    streaming: false,
                    max_context_tokens: None,
                    capability_source: "unresolved".to_string(),
                    ignored_overrides: Vec::new(),
                    drift: Vec::new(),
                },
            };

            if let (Some(observed), true) = (observed.as_ref(), row.resolved) {
                row.drift = drift_for(&row, observed);
            }
            rows.push(row);
        }

        let vision_blind = !rows.is_empty() && rows.iter().all(|r| !r.vision_in);
        if vision_blind {
            vision_blind_lanes.push(route.name.clone());
        }
        lanes.push(CapabilityLane {
            route: route.name.clone(),
            inbound_model: route.match_.model.clone(),
            vision_blind,
            targets: rows,
        });
    }

    let drift_count = lanes
        .iter()
        .flat_map(|l| &l.targets)
        .map(|t| t.drift.len())
        .sum();
    let ignored_override_count = lanes
        .iter()
        .flat_map(|l| &l.targets)
        .map(|t| t.ignored_overrides.len())
        .sum();

    Ok(CapabilityMatrixSummary {
        lane_count: lanes.len(),
        lanes,
        vision_blind_lanes,
        vision_degrade: cfg.server.vision_degrade,
        drift_source: registry_path.map(|p| p.display().to_string()),
        drift_count,
        ignored_override_count,
    })
}

/// A model registry entry as produced by the registry refresh tooling from live
/// provider metadata (OpenRouter `/models`, NVIDIA build catalog, …).
#[derive(Debug, Clone, Deserialize)]
struct RegistryModel {
    id: String,
    #[serde(default)]
    vision: Option<bool>,
    #[serde(default)]
    tool_calling: Option<bool>,
    #[serde(default)]
    json_schema: Option<bool>,
    #[serde(default)]
    context_window: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelRegistryFile {
    #[serde(default)]
    models: Vec<RegistryModel>,
}

/// Registry lookups, indexed two ways.
///
/// Cross-naming between a subscription endpoint's model id (`kimi-coding/k3`)
/// and a public catalog id (`moonshotai/kimi-k3`) is NOT derivable — the vendor
/// prefix and the marketing name are independent. So this deliberately does no
/// fuzzy matching: a target that matches nothing produces no drift rows at all.
/// Reporting a guess would be worse than reporting nothing, because a column
/// that cries wolf gets ignored, and this column exists to catch exactly the
/// silent-wrong-flag failure that made a lane reject images it could accept.
struct ModelRegistryIndex {
    by_full_id: BTreeMap<String, RegistryModel>,
    /// Trailing model segment -> entry, ONLY where that segment is unique in
    /// the registry. Ambiguous segments are dropped rather than guessed.
    by_leaf: BTreeMap<String, RegistryModel>,
}

fn load_model_registry(path: &Path) -> anyhow::Result<ModelRegistryIndex> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read model registry {}: {e}", path.display()))?;
    let parsed: ModelRegistryFile = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("cannot parse model registry {}: {e}", path.display()))?;

    let mut by_full_id = BTreeMap::new();
    let mut leaf_counts: BTreeMap<String, usize> = BTreeMap::new();
    for m in &parsed.models {
        let full = normalize_model_id(&m.id);
        *leaf_counts.entry(leaf_of(&full)).or_default() += 1;
    }
    let mut by_leaf = BTreeMap::new();
    for m in parsed.models {
        let full = normalize_model_id(&m.id);
        let leaf = leaf_of(&full);
        if leaf_counts.get(&leaf) == Some(&1) {
            by_leaf.insert(leaf, m.clone());
        }
        by_full_id.insert(full, m);
    }
    Ok(ModelRegistryIndex {
        by_full_id,
        by_leaf,
    })
}

/// The last `/`-delimited segment of a normalized model id.
fn leaf_of(id: &str) -> String {
    id.rsplit_once('/')
        .map(|(_, l)| l.to_string())
        .unwrap_or_else(|| id.to_string())
}

/// Registry ids are bare upstream ids (`moonshotai/kimi-k2.6`); target ids are
/// `provider/model` where the model may itself contain slashes. Compare on the
/// trailing model segment, lowercased, with the registry's `~` freshness marker
/// and any `:free`/`:beta` variant suffix removed.
fn normalize_model_id(id: &str) -> String {
    id.trim_start_matches('~')
        .split(':')
        .next()
        .unwrap_or(id)
        .to_ascii_lowercase()
}

/// The model portion of a target id, as the registry would name it.
fn model_key(target_id: &str) -> String {
    let model = target_id
        .split_once('/')
        .map(|(_, m)| m)
        .unwrap_or(target_id);
    normalize_model_id(model)
}

fn drift_for(row: &CapabilityRow, observed: &ModelRegistryIndex) -> Vec<String> {
    let key = model_key(&row.target_id);
    let Some(entry) = observed
        .by_full_id
        .get(&key)
        .or_else(|| observed.by_leaf.get(&leaf_of(&key)))
    else {
        return Vec::new();
    };
    let mut drift = Vec::new();
    // Only report the direction that MATTERS: the registry says the model can do
    // something the router refuses to route to it. The opposite direction
    // (declared-yes, registry-silent) is routinely correct — a subscription
    // endpoint may expose more than a public catalog knows about — so flagging
    // it would train the operator to ignore this column.
    if entry.vision == Some(true) && !row.vision_in {
        drift.push(format!(
            "vision_in declared false but registry `{}` reports vision support",
            entry.id
        ));
    }
    if entry.tool_calling == Some(true) && !row.tool_calling {
        drift.push(format!(
            "tool_calling declared false but registry `{}` reports tool support",
            entry.id
        ));
    }
    if entry.json_schema == Some(true) && !row.json_schema {
        drift.push(format!(
            "json_schema declared false but registry `{}` reports structured output",
            entry.id
        ));
    }
    if let (Some(observed_ctx), Some(declared_ctx)) = (entry.context_window, row.max_context_tokens)
    {
        if observed_ctx > declared_ctx {
            drift.push(format!(
                "max_context_tokens declared {declared_ctx} but registry `{}` reports {observed_ctx}",
                entry.id
            ));
        }
    }
    drift
}

/// Human-readable rendering. JSON stays available via `--json`.
pub(crate) fn render_capability_matrix(summary: &CapabilityMatrixSummary) -> String {
    let mut out = String::new();
    for lane in &summary.lanes {
        let model = lane.inbound_model.as_deref().unwrap_or("*");
        out.push_str(&format!("{}  (model={model})\n", lane.route));
        for t in &lane.targets {
            if !t.resolved {
                out.push_str(&format!("  {:<44} UNRESOLVED\n", t.target_id));
                continue;
            }
            let flag = |b: bool| if b { "yes" } else { "no " };
            let sources = if t.vision_sources.is_empty() {
                "any".to_string()
            } else {
                t.vision_sources.join("+")
            };
            let ctx = t
                .max_context_tokens
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_string());
            out.push_str(&format!(
                "  {:<44} vision={} ({:<24}) tools={} json={} stream={} ctx={:<8} [{}]\n",
                t.target_id,
                flag(t.vision_in),
                sources,
                flag(t.tool_calling),
                flag(t.json_schema),
                flag(t.streaming),
                ctx,
                t.capability_source
            ));
            for i in &t.ignored_overrides {
                out.push_str(&format!("  {:<44} IGNORED OVERRIDE: {i}\n", ""));
            }
            for d in &t.drift {
                out.push_str(&format!("  {:<44} DRIFT: {d}\n", ""));
            }
        }
        if lane.vision_blind {
            let verdict = if summary.vision_degrade {
                "image requests are served as TEXT (server.vision_degrade=true)"
            } else {
                "image requests FAIL with 400 (server.vision_degrade=false)"
            };
            out.push_str(&format!("  -> vision-blind lane: {verdict}\n"));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "lanes={} vision_blind={} drift={} ignored_overrides={}\n",
        summary.lane_count,
        summary.vision_blind_lanes.len(),
        summary.drift_count,
        summary.ignored_override_count
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_kimi_shaped_lane(vision_override: Option<bool>) -> Config {
        let vision_line = match vision_override {
            Some(v) => format!("    capabilities:\n      vision_in: {v}\n"),
            None => String::new(),
        };
        let yaml = format!(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: kimi-coding
    name: kimi
    type: openai_compatible
    base_url: https://api.example.test/v1
    api_key: test
{vision_line}routes:
  - name: kimi-k3
    match:
      model: kimi/k3
    targets:
      - kimi-coding/k3
"#
        );
        Config::from_yaml(&yaml).expect("config parses")
    }

    /// The reported defect, made visible: an OpenAI-compatible lane inherits
    /// `vision_in: false` from the api-kind default and nothing said so.
    #[test]
    fn matrix_names_a_vision_blind_lane_and_its_capability_source() {
        let cfg = cfg_with_kimi_shaped_lane(None);
        let matrix = capability_matrix(&cfg, None).expect("matrix builds");

        assert_eq!(matrix.lane_count, 1);
        let lane = &matrix.lanes[0];
        assert!(lane.vision_blind);
        assert_eq!(matrix.vision_blind_lanes, vec!["kimi-k3".to_string()]);
        let row = &lane.targets[0];
        assert_eq!(row.target_id, "kimi-coding/k3");
        assert!(!row.vision_in);
        assert_eq!(
            row.capability_source, "api_kind_default",
            "an inherited default must not read as an operator decision"
        );
    }

    /// The fix an operator applies: declare the capability, and the lane stops
    /// being vision-blind.
    #[test]
    fn declaring_vision_clears_the_blind_lane_and_marks_the_source() {
        let cfg = cfg_with_kimi_shaped_lane(Some(true));
        let matrix = capability_matrix(&cfg, None).expect("matrix builds");

        let lane = &matrix.lanes[0];
        assert!(!lane.vision_blind);
        assert!(lane.targets[0].vision_in);
        assert_eq!(lane.targets[0].capability_source, "provider_override");
        assert!(matrix.vision_blind_lanes.is_empty());
    }

    /// The trap that makes a correct fix look broken: a `catalog:` model row
    /// wins over the provider override, so the operator's edit does nothing and
    /// the lane keeps refusing images. The matrix must SAY so.
    #[test]
    fn matrix_reports_an_override_the_catalog_silently_overrode() {
        let yaml = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: kimi-coding
    name: kimi
    type: openai_compatible
    base_url: https://api.example.test/v1
    api_key: test
    capabilities:
      vision_in: true
catalog:
  providers:
    - id: kimi-coding
      name: kimi
      api_kind: open_ai_compatible
  models:
    - id: k3
      provider_id: kimi-coding
      context_window: 262144
      modalities: [text_in, text_out]
routes:
  - name: kimi-k3
    match:
      model: kimi/k3
    targets:
      - kimi-coding/k3
"#;
        let cfg = Config::from_yaml(yaml).expect("config parses");
        let matrix = capability_matrix(&cfg, None).expect("matrix builds");

        let row = &matrix.lanes[0].targets[0];
        assert!(!row.vision_in, "the catalog row wins");
        assert_eq!(
            row.ignored_overrides,
            vec!["vision_in: declared true but resolves to false".to_string()],
            "a declaration that had no effect must be reported, not hidden"
        );
        assert_eq!(matrix.ignored_override_count, 1);
        assert!(matrix.lanes[0].vision_blind);
    }

    #[test]
    fn matrix_renders() {
        let cfg = cfg_with_kimi_shaped_lane(None);
        let matrix = capability_matrix(&cfg, None).expect("matrix builds");
        let rendered = render_capability_matrix(&matrix);
        assert!(rendered.contains("kimi-k3"));
        assert!(rendered.contains("kimi-coding/k3"));
        assert!(rendered.contains("vision=no"));
        assert!(rendered.contains("vision-blind lane"));
        assert!(rendered.contains("lanes=1"));
        assert!(rendered.contains("ignored_overrides=0"));
    }

    /// Drift is the regression guard: the registry (built from live provider
    /// metadata) knows this model takes images, the config says it does not.
    #[test]
    fn check_drift_flags_a_capability_the_registry_reports_but_config_denies() {
        let dir = std::env::temp_dir().join(format!("sb-capmatrix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join("model-registry.json");
        std::fs::write(
            &path,
            r#"{"models":[{"id":"moonshotai/k3","name":"Kimi K3","context_window":262144,
                "tool_calling":true,"json_schema":true,"vision":true,
                "modalities":["text_in","text_out","vision_in"]}]}"#,
        )
        .expect("write registry");

        let cfg = cfg_with_kimi_shaped_lane(None);
        let matrix = capability_matrix(&cfg, Some(&path)).expect("matrix builds");

        let row = &matrix.lanes[0].targets[0];
        assert!(
            row.drift.iter().any(|d| d.contains("vision_in")),
            "drift must name the vision mismatch, got {:?}",
            row.drift
        );
        assert!(matrix.drift_count >= 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A declared capability the registry does not mention is NOT drift — a
    /// subscription endpoint legitimately exposes more than a public catalog.
    #[test]
    fn check_drift_is_silent_when_config_declares_more_than_the_registry() {
        let dir = std::env::temp_dir().join(format!("sb-capmatrix-q-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join("model-registry.json");
        std::fs::write(
            &path,
            r#"{"models":[{"id":"moonshotai/k3","vision":false,"tool_calling":false}]}"#,
        )
        .expect("write registry");

        let cfg = cfg_with_kimi_shaped_lane(Some(true));
        let matrix = capability_matrix(&cfg, Some(&path)).expect("matrix builds");

        assert_eq!(matrix.drift_count, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An ambiguous leaf name must NOT be guessed at. Two vendors shipping a
    /// model called `k3` means the registry cannot say which one this lane is,
    /// and a wrong drift row is worse than none.
    #[test]
    fn check_drift_refuses_to_guess_an_ambiguous_leaf_name() {
        let dir = std::env::temp_dir().join(format!("sb-capmatrix-amb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let path = dir.join("model-registry.json");
        std::fs::write(
            &path,
            r#"{"models":[{"id":"moonshotai/k3","vision":true},
                         {"id":"someoneelse/k3","vision":true}]}"#,
        )
        .expect("write registry");

        let cfg = cfg_with_kimi_shaped_lane(None);
        let matrix = capability_matrix(&cfg, Some(&path)).expect("matrix builds");

        assert_eq!(
            matrix.drift_count, 0,
            "an ambiguous leaf must produce no drift row"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_keys_normalize_variant_and_freshness_markers() {
        assert_eq!(
            model_key("openrouter/moonshotai/kimi-k2.6:free"),
            "moonshotai/kimi-k2.6"
        );
        assert_eq!(
            normalize_model_id("~moonshotai/kimi-latest"),
            "moonshotai/kimi-latest"
        );
    }
}
