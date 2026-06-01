/// Install the tracing subscriber: an env-filtered fmt layer that prints span
/// closes (so the request/attempt span tree is visible), plus — when built with
/// the `otel` feature and an OTLP endpoint is configured — an OpenTelemetry
/// export layer. The spans are the same either way; OTel just ships them out.
pub(crate) fn init_tracing(otel_endpoint: Option<&str>) {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_writer(std::io::stderr);

    #[cfg(feature = "otel")]
    {
        let otel_layer = match otel_endpoint {
            Some(endpoint) => match otel_export::build_tracer(endpoint) {
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
        if otel_endpoint.is_some() {
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
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig;

    pub fn build_tracer(endpoint: &str) -> Result<opentelemetry_sdk::trace::Tracer, String> {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
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
