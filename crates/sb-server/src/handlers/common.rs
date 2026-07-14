use axum::http::HeaderMap;
use sb_trace::{
    NATIVE_EXECUTION_LANE_ID_META, NATIVE_EXECUTION_LANE_REVISION_META,
    NATIVE_EXECUTION_OBSERVED_EFFORT_META, NATIVE_EXECUTION_OBSERVED_PATH_META,
    NATIVE_EXECUTION_REQUESTED_EFFORT_META,
};

const LANE_ID_HEADER: &str = "x-switchback-lane-id";
const LANE_REVISION_HEADER: &str = "x-switchback-lane-revision";
const REQUESTED_EFFORT_HEADER: &str = "x-switchback-requested-effort";

fn bounded_token(value: &str, max_len: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > max_len
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        return None;
    }
    Some(value.to_string())
}

fn valid_lane_revision(value: &str) -> Option<String> {
    let digest = value.trim().strip_prefix("sha256:")?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("sha256:{}", digest.to_ascii_lowercase()))
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_string)
}

fn observed_effort(body: &serde_json::Value) -> Option<(String, String)> {
    const POINTERS: &[&str] = &[
        "/reasoning/effort",
        "/output_config/effort",
        "/reasoning_effort",
        "/effort",
        "/response/reasoning/effort",
        "/response/output_config/effort",
        "/response/reasoning_effort",
        "/response/effort",
    ];
    POINTERS.iter().find_map(|path| {
        bounded_token(body.pointer(path)?.as_str()?, 32).map(|effort| (effort, (*path).to_string()))
    })
}

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

pub(crate) fn attach_native_execution_metadata(
    req: &mut sb_core::AiRequest,
    headers: &HeaderMap,
    body: &serde_json::Value,
) {
    if let Some(value) =
        header_value(headers, LANE_ID_HEADER).and_then(|value| bounded_token(&value, 128))
    {
        req.metadata
            .insert(NATIVE_EXECUTION_LANE_ID_META.to_string(), value);
    }
    if let Some(value) =
        header_value(headers, LANE_REVISION_HEADER).and_then(|value| valid_lane_revision(&value))
    {
        req.metadata
            .insert(NATIVE_EXECUTION_LANE_REVISION_META.to_string(), value);
    }
    if let Some(value) =
        header_value(headers, REQUESTED_EFFORT_HEADER).and_then(|value| bounded_token(&value, 32))
    {
        req.metadata
            .insert(NATIVE_EXECUTION_REQUESTED_EFFORT_META.to_string(), value);
    }
    if let Some((effort, path)) = observed_effort(body) {
        req.metadata
            .insert(NATIVE_EXECUTION_OBSERVED_EFFORT_META.to_string(), effort);
        req.metadata
            .insert(NATIVE_EXECUTION_OBSERVED_PATH_META.to_string(), path);
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use sb_core::{AiRequest, Message};

    use super::*;

    #[test]
    fn native_execution_metadata_joins_lane_declaration_to_wire_effort() {
        let mut headers = HeaderMap::new();
        headers.insert(LANE_ID_HEADER, HeaderValue::from_static("gpt56-sol-ultra"));
        headers.insert(
            LANE_REVISION_HEADER,
            HeaderValue::from_static(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
        );
        headers.insert(REQUESTED_EFFORT_HEADER, HeaderValue::from_static("ultra"));
        let body = serde_json::json!({"reasoning": {"effort": "ultra"}});
        let mut req = AiRequest::new("gpt-5.6-sol", vec![Message::user("hi")]);

        attach_native_execution_metadata(&mut req, &headers, &body);

        assert_eq!(
            req.metadata
                .get(NATIVE_EXECUTION_LANE_ID_META)
                .map(String::as_str),
            Some("gpt56-sol-ultra")
        );
        assert_eq!(
            req.metadata
                .get(NATIVE_EXECUTION_LANE_REVISION_META)
                .map(String::as_str),
            Some("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            req.metadata
                .get(NATIVE_EXECUTION_REQUESTED_EFFORT_META)
                .map(String::as_str),
            Some("ultra")
        );
        assert_eq!(
            req.metadata
                .get(NATIVE_EXECUTION_OBSERVED_EFFORT_META)
                .map(String::as_str),
            Some("ultra")
        );
        assert_eq!(
            req.metadata
                .get(NATIVE_EXECUTION_OBSERVED_PATH_META)
                .map(String::as_str),
            Some("/reasoning/effort")
        );
    }

    #[test]
    fn malformed_lane_revision_is_not_recorded_as_execution_evidence() {
        let mut headers = HeaderMap::new();
        headers.insert(LANE_ID_HEADER, HeaderValue::from_static("gpt56-sol-ultra"));
        headers.insert(
            LANE_REVISION_HEADER,
            HeaderValue::from_static("sha256:not-a-valid-digest"),
        );
        headers.insert(REQUESTED_EFFORT_HEADER, HeaderValue::from_static("ultra"));
        let mut req = AiRequest::new("gpt-5.6-sol", vec![Message::user("hi")]);

        attach_native_execution_metadata(&mut req, &headers, &serde_json::json!({}));

        assert!(!req
            .metadata
            .contains_key(NATIVE_EXECUTION_LANE_REVISION_META));
        assert_eq!(
            req.metadata
                .get(NATIVE_EXECUTION_REQUESTED_EFFORT_META)
                .map(String::as_str),
            Some("ultra")
        );
    }
}
