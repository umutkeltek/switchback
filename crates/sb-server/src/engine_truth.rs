//! What a loaded model really runs with, read from the engine's own
//! listing (its argument list) and, on request, from the model's own
//! `/props`: the set context, the slots, whether the slots share one pool
//! (kv-unified), the context one request can use, the KV cache types, and
//! whether weights may sit on the CPU.
//!
//! A llama.cpp server splits `--ctx-size` over `--parallel` slots unless
//! `--kv-unified` is on, so a model "set to" 98304 with 3 slots serves at
//! most 32768 tokens per request. The settings view shows the set value;
//! this view shows what one request gets.
//!
//! Reading the listing never calls a machine. `/props` is read only when an
//! operator asks for it (`GET /v1/models/{model}/engine-props`), only for a
//! model that is loaded, and never in the background: a call to a model's
//! process counts as activity and keeps it awake.

use serde_json::{json, Value};

/// A flag's last value in an argument list (`--name value` or
/// `--name=value`); a flag with no value after it reads "on".
fn arg_value(args: &[&str], names: &[&str]) -> Option<String> {
    let mut out = None;
    for (i, arg) in args.iter().enumerate() {
        for name in names {
            if arg == name {
                out = Some(match args.get(i + 1) {
                    Some(v) if !v.starts_with('-') || v.parse::<f64>().is_ok() => v.to_string(),
                    _ => "on".to_string(),
                });
            } else if let Some(v) = arg.strip_prefix(&format!("{name}=")) {
                out = Some(v.to_string());
            }
        }
    }
    out
}

fn number(text: Option<&str>) -> Option<u64> {
    text.and_then(|t| t.parse::<f64>().ok())
        .filter(|n| *n >= 0.0)
        .map(|n| n as u64)
}

fn has(args: &[&str], names: &[&str]) -> bool {
    args.iter().any(|arg| names.contains(arg))
}

fn switch_on(value: Option<&str>) -> Option<bool> {
    match value?.to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "enabled" => Some(true),
        "off" | "false" | "0" | "disabled" => Some(false),
        _ => None,
    }
}

/// The truth view of a listing row (`status.args`, the engine's command
/// line for the model), or `Null` when the engine lists no arguments.
pub(crate) fn effective(row: &Value) -> Value {
    let args: Vec<&str> = row["status"]["args"]
        .as_array()
        .map(|args| args.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if args.is_empty() {
        return Value::Null;
    }
    effective_from_args(&args)
}

/// The truth view of an argument list.
pub(crate) fn effective_from_args(args: &[&str]) -> Value {
    let ctx = number(arg_value(args, &["--ctx-size", "-c"]).as_deref());
    let slots = number(arg_value(args, &["--parallel", "-np"]).as_deref());
    let unified = if has(args, &["--no-kv-unified", "-no-kvu"]) {
        Some(false)
    } else if has(args, &["--kv-unified", "-kvu"]) {
        Some(true)
    } else if slots.is_some() {
        Some(false)
    } else {
        None // the engine picks its slots and their sharing itself
    };
    let per_request = match (ctx, slots, unified) {
        (Some(ctx), _, Some(true)) => Some(ctx),
        (Some(ctx), Some(slots), _) => Some(ctx / slots.max(1)),
        _ => None,
    };
    let ngl = arg_value(args, &["--n-gpu-layers", "--gpu-layers", "-ngl"]);
    let fit = switch_on(arg_value(args, &["--fit", "-fit"]).as_deref());
    let n_cpu_moe = number(arg_value(args, &["--n-cpu-moe", "-ncmoe"]).as_deref()).unwrap_or(0);
    let cpu_moe = has(args, &["--cpu-moe", "-cmoe"]);
    let cpu_overrides = args
        .iter()
        .enumerate()
        .filter_map(|(i, arg)| match *arg {
            "--override-tensor" | "-ot" => args.get(i + 1).copied(),
            other => other.strip_prefix("--override-tensor="),
        })
        .filter(|rule| rule.to_ascii_lowercase().contains("=cpu"))
        .count();
    let all_layers = ngl
        .as_deref()
        .and_then(|n| n.parse::<f64>().ok())
        .is_some_and(|n| n >= 999.0)
        || matches!(ngl.as_deref(), Some("all" | "auto"));
    let cpu_weights = if n_cpu_moe > 0 || cpu_moe || cpu_overrides > 0 {
        "yes"
    } else if (ngl.is_some() && !all_layers) || fit == Some(true) {
        // Some layers may stay on the CPU: by the model's layer count, or
        // because the engine fits the model to the free memory.
        "maybe"
    } else if ngl.is_some() || fit == Some(false) {
        "no"
    } else {
        "unknown"
    };
    json!({
        "source": "args",
        "ctx_size": ctx,
        "slots": slots,
        "kv_unified": unified,
        "per_request_ctx": per_request,
        "pool_ctx": ctx,
        "cache_type_k": arg_value(args, &["--cache-type-k", "-ctk"]).unwrap_or_else(|| "f16".into()),
        "cache_type_v": arg_value(args, &["--cache-type-v", "-ctv"]).unwrap_or_else(|| "f16".into()),
        "batch_size": number(arg_value(args, &["--batch-size", "-b"]).as_deref()),
        "ubatch_size": number(arg_value(args, &["--ubatch-size", "-ub"]).as_deref()),
        "n_gpu_layers": ngl,
        "fit": fit,
        "n_cpu_moe": n_cpu_moe,
        "cpu_moe": cpu_moe,
        "cpu_tensor_overrides": cpu_overrides,
        "cpu_weights": cpu_weights,
        "projector": args.iter().any(|arg| *arg == "--mmproj" || *arg == "-mm" || arg.starts_with("--mmproj=")),
    })
}

/// Why one request cannot use the whole set context, or `None`.
pub(crate) fn split_finding(effective: &Value) -> Option<String> {
    let ctx = effective["ctx_size"].as_u64()?;
    let per = effective["per_request_ctx"].as_u64()?;
    let slots = effective["slots"].as_u64().unwrap_or(1);
    (per < ctx).then(|| {
        format!(
            "the set context {ctx} is split over {slots} slots (kv-unified off): one request can \
             use {per}"
        )
    })
}

/// The model's own numbers from its `/props` (llama.cpp:
/// `default_generation_settings.n_ctx` is one slot's context,
/// `total_slots` the slot count, `modalities.vision` a projector).
pub(crate) fn from_props(props: &Value) -> Value {
    let settings = &props["default_generation_settings"];
    json!({
        "n_ctx_per_slot": settings["n_ctx"].as_u64().or_else(|| props["n_ctx"].as_u64()),
        "slots": props["total_slots"].as_u64(),
        "vision": props["modalities"]["vision"].as_bool(),
        "build": props["build_info"].as_str(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(args: &[&str]) -> Value {
        json!({"id": "m", "status": {"value": "loaded", "args": args}})
    }

    #[test]
    fn a_set_context_is_split_over_the_slots_without_kv_unified() {
        // The night model on 2026-11-23, revision 54: 98304 over 3 slots.
        let args = [
            "llama-server",
            "--ctx-size",
            "98304",
            "--parallel",
            "3",
            "--cache-type-k",
            "q8_0",
            "--cache-type-v",
            "q8_0",
            "--fit",
            "off",
            "--n-gpu-layers",
            "999",
        ];
        let view = effective(&row(&args));
        assert_eq!(view["per_request_ctx"], 32768);
        assert_eq!(view["kv_unified"], false);
        assert_eq!(view["slots"], 3);
        assert_eq!(view["cache_type_k"], "q8_0");
        assert_eq!(view["cpu_weights"], "no");
        assert!(split_finding(&view)
            .unwrap()
            .contains("one request can use 32768"));
    }

    #[test]
    fn kv_unified_gives_one_request_the_whole_pool() {
        let view = effective_from_args(&[
            "s",
            "--ctx-size",
            "131072",
            "--parallel",
            "3",
            "--kv-unified",
        ]);
        assert_eq!(view["per_request_ctx"], 131072);
        assert_eq!(split_finding(&view), None);
        let off = effective_from_args(&[
            "s",
            "-c",
            "8192",
            "-np",
            "2",
            "--kv-unified",
            "--no-kv-unified",
        ]);
        assert_eq!(off["per_request_ctx"], 4096);
    }

    #[test]
    fn weights_on_the_cpu_are_named() {
        let base = ["s", "--ctx-size", "8192"];
        let with = |extra: &[&str]| {
            let mut args = base.to_vec();
            args.extend_from_slice(extra);
            effective_from_args(&args)["cpu_weights"].clone()
        };
        assert_eq!(with(&["--n-cpu-moe", "4"]), "yes");
        assert_eq!(with(&["-ot", "blk\\.1\\.ffn=CPU"]), "yes");
        assert_eq!(with(&["--fit", "on"]), "maybe");
        assert_eq!(with(&["--n-gpu-layers", "20"]), "maybe");
        assert_eq!(with(&["--n-gpu-layers", "999", "--fit", "off"]), "no");
        assert_eq!(with(&[]), "unknown");
    }

    #[test]
    fn no_arguments_is_no_view() {
        assert_eq!(
            effective(&json!({"id": "m", "status": {"value": "unloaded"}})),
            Value::Null
        );
    }

    #[test]
    fn props_give_one_slots_context() {
        let props = json!({"default_generation_settings": {"n_ctx": 32768}, "total_slots": 3,
                           "modalities": {"vision": true}, "build_info": "b1-x"});
        let view = from_props(&props);
        assert_eq!(view["n_ctx_per_slot"], 32768);
        assert_eq!(view["slots"], 3);
        assert_eq!(view["vision"], true);
    }
}
