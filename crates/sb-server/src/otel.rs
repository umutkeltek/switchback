use std::collections::HashMap;

use base64::Engine as _;

/// Resolved OTLP export settings. Headers are metadata-only configuration; the
/// Langfuse Authorization value is built from env at startup and never logged.
#[derive(Debug, Clone, Default)]
pub(crate) struct OtlpExportConfig {
    pub(crate) endpoint: Option<String>,
    pub(crate) headers: HashMap<String, String>,
}

pub(crate) fn otlp_export_config(config: Option<&sb_core::Config>) -> OtlpExportConfig {
    let Some(config) = config else {
        return OtlpExportConfig::default();
    };
    let mut export = OtlpExportConfig {
        endpoint: config.server.otel_endpoint.clone(),
        headers: HashMap::new(),
    };
    if config.server.langfuse.enabled {
        let endpoint = config
            .server
            .langfuse
            .otel_endpoint
            .clone()
            .unwrap_or_else(|| {
                format!(
                    "{}/api/public/otel/v1/traces",
                    config.server.langfuse.host.trim_end_matches('/')
                )
            });
        export.endpoint = Some(endpoint);
        match (
            std::env::var(&config.server.langfuse.public_key_env),
            std::env::var(&config.server.langfuse.secret_key_env),
        ) {
            (Ok(public), Ok(secret)) if !public.trim().is_empty() && !secret.trim().is_empty() => {
                let auth = base64::engine::general_purpose::STANDARD.encode(format!(
                    "{}:{}",
                    public.trim(),
                    secret.trim()
                ));
                export
                    .headers
                    .insert("Authorization".to_string(), format!("Basic {auth}"));
                export
                    .headers
                    .insert("x-langfuse-ingestion-version".to_string(), "4".to_string());
            }
            _ => {
                eprintln!(
                    "server.langfuse.enabled=true but {} / {} are not both set; OTLP export will likely be rejected",
                    config.server.langfuse.public_key_env,
                    config.server.langfuse.secret_key_env
                );
            }
        }
    }
    export
}

/// Install the tracing subscriber: an env-filtered fmt layer that prints span
/// closes (so the request/attempt span tree is visible), plus — when built with
/// the `otel` feature and an OTLP endpoint is configured — an OpenTelemetry
/// export layer. The spans are the same either way; OTel just ships them out.
pub(crate) fn init_tracing(export: OtlpExportConfig) {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_writer(std::io::stderr);

    #[cfg(feature = "otel")]
    {
        let otel_layer = match export.endpoint.as_deref() {
            Some(endpoint) => match otel_export::build_tracer(endpoint, export.headers) {
                Ok(tracer) => {
                    tracing::info!(%endpoint, "otel: exporting spans via OTLP");
                    Some(tracing_opentelemetry::layer().with_tracer(tracer))
                }
                Err(e) => {
                    eprintln!("otel: {e}; export disabled (spans still render locally)");
                    None
                }
            },
            None => None,
        };
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .with(otel_layer)
            .try_init();
    }

    #[cfg(not(feature = "otel"))]
    {
        if export.endpoint.is_some() {
            eprintln!("otel_endpoint is set but this binary was built without the `otel` feature");
        }
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .try_init();
    }
}

/// OTLP exporter wiring. Builds a batch span exporter over OTLP/HTTP and a
/// tracer the `tracing-opentelemetry` layer drives. Only compiled with `otel`.
#[cfg(feature = "otel")]
mod otel_export {
    use std::collections::HashMap;

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};

    pub fn build_tracer(
        endpoint: &str,
        headers: HashMap<String, String>,
    ) -> Result<opentelemetry_sdk::trace::Tracer, String> {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .with_headers(headers)
            .build()
            .map_err(|e| format!("build OTLP exporter: {e}"))?;
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name("switchback")
                    .build(),
            )
            .build();
        let tracer = provider.tracer("switchback");
        // Keep the provider installed globally so the batch exporter keeps
        // flushing for the process lifetime.
        opentelemetry::global::set_tracer_provider(provider);
        Ok(tracer)
    }
}
