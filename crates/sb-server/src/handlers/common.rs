use axum::http::HeaderMap;

pub(crate) fn session_id_from_headers(headers: &HeaderMap) -> Option<String> {
    [
        "x-switchback-session-id",
        "x-codex-session-id",
        "x-session-id",
    ]
    .iter()
    .find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

pub(crate) fn attach_session_metadata(req: &mut sb_core::AiRequest, headers: &HeaderMap) {
    if req.metadata.contains_key("session_id") {
        return;
    }
    if let Some(session_id) = session_id_from_headers(headers) {
        req.metadata.insert("session_id".to_string(), session_id);
    }
}

pub(crate) fn attach_native_client_metadata(
    req: &mut sb_core::AiRequest,
    headers: &HeaderMap,
    default_profile: &str,
    client_protocol: &str,
) -> String {
    let (profile, source) = headers
        .get("x-switchback-client-profile")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map(|value| (value.to_string(), "header"))
        .unwrap_or_else(|| (default_profile.to_string(), "default"));
    req.metadata
        .insert("client_profile".to_string(), profile.clone());
    req.metadata
        .insert("client_profile_source".to_string(), source.to_string());
    req.metadata
        .insert("client_protocol".to_string(), client_protocol.to_string());
    profile
}
