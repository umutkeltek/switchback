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
