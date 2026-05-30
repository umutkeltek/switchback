//! Tier-2 sandboxed Wasm plugins (Oracle #6), behind the `wasm` build feature.
//!
//! A [`WasmPlugin`] runs a `.wasm`/`.wat` module that implements the `pre_route`
//! hook in a Wasmtime sandbox. The host ABI is intentionally tiny (no JSON in the
//! guest): the guest exports
//!   - `memory`                       — linear memory
//!   - `alloc(size: i32) -> i32`      — return a writable buffer pointer
//!   - `pre_route(ptr,len) -> i32`    — given the model bytes, return 0 to allow
//!                                       or an HTTP status (e.g. 403) to reject.
//!
//! The same `Module` is reused across requests; a fresh `Store`/`Instance` is
//! created per call, so the guest is stateless and the plugin is `Send + Sync`.
//! (Per-call instantiation is cheap for small modules; richer hooks + the WIT
//! component model are a follow-up.)

use sb_core::AiRequest;
use wasmtime::{Engine, Instance, Module, Store};

use crate::{Plugin, PluginOutcome};

pub struct WasmPlugin {
    engine: Engine,
    module: Module,
    label: String,
}

impl WasmPlugin {
    /// Compile a `.wasm`/`.wat` module from disk (the publish-time "prepare").
    pub fn load(path: &str) -> Result<Self, String> {
        let engine = Engine::default();
        let module = Module::from_file(&engine, path).map_err(|e| e.to_string())?;
        let stem = std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin");
        Ok(Self {
            engine,
            module,
            label: format!("wasm:{stem}"),
        })
    }

    /// Run the guest `pre_route` over the model string. Returns the guest's
    /// status code (0 = allow).
    fn run_pre_route(&self, model: &str) -> Result<i32, String> {
        let mut store = Store::new(&self.engine, ());
        let instance =
            Instance::new(&mut store, &self.module, &[]).map_err(|e| e.to_string())?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or("guest does not export `memory`")?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|e| e.to_string())?;
        let pre_route = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "pre_route")
            .map_err(|e| e.to_string())?;

        let len = model.len() as i32;
        let ptr = alloc.call(&mut store, len).map_err(|e| e.to_string())?;
        memory
            .write(&mut store, ptr as usize, model.as_bytes())
            .map_err(|e| e.to_string())?;
        pre_route
            .call(&mut store, (ptr, len))
            .map_err(|e| e.to_string())
    }
}

impl Plugin for WasmPlugin {
    fn name(&self) -> &str {
        &self.label
    }

    fn pre_route(&self, req: &mut AiRequest) -> PluginOutcome {
        match self.run_pre_route(&req.model) {
            Ok(0) => PluginOutcome::Continue,
            Ok(code) => PluginOutcome::Reject {
                // Clamp to a sane HTTP error range so a guest can't return junk.
                status: code.clamp(400, 599) as u16,
                message: format!("rejected by wasm plugin `{}`", self.label),
            },
            Err(e) => {
                // Fail-open on a guest trap/error — don't wedge the request path.
                tracing::warn!(error = %e, plugin = %self.label, "wasm pre_route errored; allowing");
                PluginOutcome::Continue
            }
        }
    }
}
