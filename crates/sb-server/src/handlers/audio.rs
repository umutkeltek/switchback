//! `/v1/audio/transcriptions`, `/v1/audio/translations` and
//! `/v1/audio/speech` in OpenAI's shapes. The edge reads the upload once
//! (bounded by `server.audio_max_upload_bytes`), splits the multipart form
//! into fields without copying the file, and hands it to the runtime, which
//! routes it like any other request. Nothing here logs or stores the audio.

use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use futures::StreamExt;
use sb_runtime::{AudioBody, AudioCall, AudioInput, AudioOutcome, FormPart};

use crate::handlers::common::session_id_from_headers;
use crate::http_response::{
    openai_error, render_exec_error, with_queue_header, with_request_id, with_revision_header,
    with_route_header,
};
use crate::{admission, tenancy, AppState};

pub(crate) async fn transcriptions(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    upload(state, principal, headers, body, AudioCall::Transcription).await
}

pub(crate) async fn translations(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    upload(state, principal, headers, body, AudioCall::Translation).await
}

pub(crate) async fn speech(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    let Some(object) = body.as_object() else {
        return bad_request("the body must be a JSON object");
    };
    let Some(model) = object
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return bad_request("missing \"model\"");
    };
    let Some(text) = object.get("input").and_then(|v| v.as_str()) else {
        return bad_request("missing \"input\": the text to speak");
    };
    let input = AudioInput {
        call: AudioCall::Speech,
        model,
        input_chars: Some(text.chars().count() as u64),
        body: AudioBody::Json(body),
        stream: false,
    };
    run(state, principal, headers, input, started).await
}

async fn upload(
    state: AppState,
    principal: tenancy::Principal,
    headers: HeaderMap,
    body: Body,
    call: AudioCall,
) -> Response {
    let started = Instant::now();
    let limit = state.snapshot().config.server.audio_max_upload_bytes;
    let Some(boundary) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(multipart_boundary)
    else {
        return bad_request("the body must be multipart/form-data with a boundary");
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > limit) {
        return too_large(limit);
    }
    let bytes = match read_limited(body, limit).await {
        Ok(bytes) => bytes,
        Err(ReadError::TooLarge) => return too_large(limit),
        Err(ReadError::Broken(message)) => return bad_request(&message),
    };
    let parts = match parse_multipart(&bytes, &boundary) {
        Ok(parts) => parts,
        Err(message) => return bad_request(&message),
    };
    let field = |name: &str| {
        parts
            .iter()
            .find(|part| part.name == name && part.filename.is_none())
            .map(|part| String::from_utf8_lossy(&part.data).trim().to_string())
    };
    if !parts.iter().any(|part| part.name == "file") {
        return bad_request("missing \"file\": the audio to transcribe");
    }
    let Some(model) = field("model").filter(|model| !model.is_empty()) else {
        return bad_request("missing \"model\"");
    };
    let stream = field("stream").is_some_and(|value| value.eq_ignore_ascii_case("true"));
    let input = AudioInput {
        call,
        model,
        body: AudioBody::Form(parts),
        stream,
        input_chars: None,
    };
    run(state, principal, headers, input, started).await
}

async fn run(
    state: AppState,
    principal: tenancy::Principal,
    headers: HeaderMap,
    input: AudioInput,
    started: Instant,
) -> Response {
    let (admit, queue_ms) = match admission::acquire(&state).await {
        Ok(slot) => slot,
        Err(resp) => return *resp,
    };
    let conc = match tenancy::admit_concurrency(&state, &principal) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };
    let (revision, outcome) = state
        .engine
        .execute_audio(
            input,
            principal.tenant,
            principal.project,
            session_id_from_headers(&headers),
            started,
        )
        .await;
    let (response, request_id) = match outcome {
        AudioOutcome::Body {
            status,
            content_type,
            body,
            summary,
            request_id,
        } => {
            // The admission and tenant slots stay taken until the last
            // byte has gone out (or the client went away).
            let body = body.map(move |chunk| {
                let _hold = (&admit, &conc);
                chunk.map_err(|error| std::io::Error::other(error.message))
            });
            let mut builder = Response::builder()
                .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
                // Audio and transcripts are never stored by a shared cache.
                .header(header::CACHE_CONTROL, "no-store");
            if let Some(content_type) = content_type {
                builder = builder.header(header::CONTENT_TYPE, content_type);
            }
            let response = match builder.body(Body::from_stream(body)) {
                Ok(response) => response,
                Err(_) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(openai_error(
                        "failed to build the audio response",
                        "upstream_error",
                    )),
                )
                    .into_response(),
            };
            (with_route_header(response, &summary), request_id)
        }
        AudioOutcome::Error { error, request_id } => (render_exec_error(&error), request_id),
    };
    with_queue_header(
        with_revision_header(with_request_id(response, &request_id), revision),
        queue_ms,
    )
}

fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(openai_error(message, "invalid_request_error")),
    )
        .into_response()
}

fn too_large(limit: u64) -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(openai_error(
            &format!(
                "the upload is larger than {limit} bytes (server.audio_max_upload_bytes); split the audio or compress it"
            ),
            "audio_too_large",
        )),
    )
        .into_response()
}

enum ReadError {
    TooLarge,
    Broken(String),
}

/// Read the whole body, stopping as soon as it passes `limit`.
async fn read_limited(body: Body, limit: u64) -> Result<Bytes, ReadError> {
    let mut stream = body.into_data_stream();
    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ReadError::Broken(format!("the upload broke off: {e}")))?;
        if (collected.len() + chunk.len()) as u64 > limit {
            return Err(ReadError::TooLarge);
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(collected))
}

/// The `boundary` parameter of a `multipart/form-data` content type.
fn multipart_boundary(content_type: &str) -> Option<String> {
    let mut params = content_type.split(';');
    let kind = params.next()?.trim();
    if !kind.eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    params.find_map(|param| {
        let (name, value) = param.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("boundary") {
            return None;
        }
        let value = value.trim().trim_matches('"');
        (!value.is_empty() && value.len() <= 200).then(|| value.to_string())
    })
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| at + from)
}

/// Split a multipart/form-data body into its parts. Each part's data is a
/// slice of `body` (no copy). Fields are kept in order, unknown ones too.
fn parse_multipart(body: &Bytes, boundary: &str) -> Result<Vec<FormPart>, String> {
    let malformed = |what: &str| format!("malformed multipart body: {what}");
    let opening = format!("--{boundary}");
    let delimiter = format!("\r\n--{boundary}");
    let mut at = if body.starts_with(opening.as_bytes()) {
        opening.len()
    } else {
        find(body, delimiter.as_bytes(), 0).ok_or_else(|| malformed("no boundary"))?
            + delimiter.len()
    };
    let mut parts = Vec::new();
    loop {
        if body[at..].starts_with(b"--") {
            return Ok(parts);
        }
        if !body[at..].starts_with(b"\r\n") {
            return Err(malformed("boundary not followed by a line break"));
        }
        at += 2;
        let headers_end =
            find(body, b"\r\n\r\n", at).ok_or_else(|| malformed("part without headers"))?;
        let head = std::str::from_utf8(&body[at..headers_end])
            .map_err(|_| malformed("part headers are not UTF-8"))?;
        let data_start = headers_end + 4;
        let data_end = find(body, delimiter.as_bytes(), data_start)
            .ok_or_else(|| malformed("no closing boundary"))?;
        let mut name = None;
        let mut filename = None;
        let mut content_type = None;
        for line in head.split("\r\n") {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim();
            if key.eq_ignore_ascii_case("content-disposition") {
                name = disposition_param(value, "name");
                filename = disposition_param(value, "filename");
            } else if key.eq_ignore_ascii_case("content-type") {
                content_type = Some(value.trim().to_string());
            }
        }
        let name = name.ok_or_else(|| malformed("part without a name"))?;
        parts.push(FormPart {
            name,
            filename,
            content_type,
            data: body.slice(data_start..data_end),
        });
        at = data_end + delimiter.len();
    }
}

/// One parameter of a `Content-Disposition: form-data; name="x"` value.
fn disposition_param(value: &str, wanted: &str) -> Option<String> {
    value.split(';').skip(1).find_map(|param| {
        let (key, value) = param.split_once('=')?;
        if !key.trim().eq_ignore_ascii_case(wanted) {
            return None;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        Some(value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_is_read_from_the_content_type() {
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=abc").as_deref(),
            Some("abc")
        );
        assert_eq!(
            multipart_boundary("Multipart/Form-Data; charset=utf-8; boundary=\"a b\"").as_deref(),
            Some("a b")
        );
        assert_eq!(multipart_boundary("application/json"), None);
        assert_eq!(multipart_boundary("multipart/form-data"), None);
    }

    #[test]
    fn parts_keep_names_files_and_bytes_without_copying() {
        let body = Bytes::from_static(
            b"--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ntranscribe\r\n\
--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
Content-Type: audio/wav\r\n\r\nRIFF\x00\xff\r\n-B\r\n\
--B\r\nContent-Disposition: form-data; name=\"num_speakers\"\r\n\r\n2\r\n--B--\r\n",
        );
        let parts = parse_multipart(&body, "B").unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].name, "model");
        assert_eq!(parts[0].data.as_ref(), b"transcribe");
        assert_eq!(parts[1].filename.as_deref(), Some("a.wav"));
        assert_eq!(parts[1].content_type.as_deref(), Some("audio/wav"));
        assert_eq!(parts[1].data.as_ref(), b"RIFF\x00\xff\r\n-B");
        assert_eq!(parts[2].name, "num_speakers");
        assert_eq!(parts[2].data.as_ref(), b"2");
    }

    #[test]
    fn a_truncated_body_is_refused() {
        let body =
            Bytes::from_static(b"--B\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\n1");
        assert!(parse_multipart(&body, "B").is_err());
    }
}
