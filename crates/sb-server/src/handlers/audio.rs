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
    body: Bytes,
) -> Response {
    let started = Instant::now();
    // Parsed here rather than by axum's `Json`, so a bad body gets an
    // OpenAI-shaped error a client can read.
    let body: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(e) => return bad_request(&format!("the body is not valid JSON: {e}")),
    };
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
    let bytes = match read_limited(body, limit, declared).await {
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
            session_id_from_headers(&state, &headers),
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

/// Read the whole body, stopping as soon as it passes `limit`. A declared
/// `Content-Length` (already checked against `limit`) sizes the buffer once.
async fn read_limited(body: Body, limit: u64, declared: Option<u64>) -> Result<Bytes, ReadError> {
    let mut stream = body.into_data_stream();
    let capacity = declared.unwrap_or(0).min(limit);
    let mut collected = Vec::with_capacity(usize::try_from(capacity).unwrap_or(0));
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
                let params = disposition_params(value);
                let param = |wanted: &str| {
                    params
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(wanted))
                        .map(|(_, value)| value.clone())
                };
                name = param("name");
                // RFC 6266/5987: `filename*` wins over `filename`.
                filename = param("filename*")
                    .and_then(|value| extended_value(&value))
                    .or_else(|| param("filename"));
            } else if key.eq_ignore_ascii_case("content-type") {
                content_type = Some(value.trim().to_string());
            }
        }
        let name = name.ok_or_else(|| malformed("part without a name"))?;
        // A client that sends the audio without a filename still sent a
        // file: forward it as one, named after its content type.
        if name == "file" && filename.is_none() {
            filename = Some(default_filename(content_type.as_deref()));
        }
        parts.push(FormPart {
            name,
            filename,
            content_type,
            data: body.slice(data_start..data_end),
        });
        at = data_end + delimiter.len();
    }
}

/// The parameters of a `Content-Disposition: form-data; name="x"; ...`
/// value, in order. A `;` inside a quoted value belongs to the value, and
/// `\"` inside quotes is a quote.
fn disposition_params(value: &str) -> Vec<(String, String)> {
    let mut params = Vec::new();
    let mut chars = value.chars().peekable();
    // Skip the disposition type (`form-data`).
    for c in chars.by_ref() {
        if c == ';' {
            break;
        }
    }
    loop {
        let mut key = String::new();
        for c in chars.by_ref() {
            if c == '=' || c == ';' {
                break;
            }
            key.push(c);
        }
        let key = key.trim().to_string();
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => {
                        if let Some(escaped) = chars.next() {
                            value.push(escaped);
                        }
                    }
                    c => value.push(c),
                }
            }
            // Anything after the closing quote up to `;` is ignored.
            for c in chars.by_ref() {
                if c == ';' {
                    break;
                }
            }
        } else {
            for c in chars.by_ref() {
                if c == ';' {
                    break;
                }
                value.push(c);
            }
            value = value.trim().to_string();
        }
        if !key.is_empty() {
            params.push((key, value));
        }
        if chars.peek().is_none() {
            return params;
        }
    }
}

/// An RFC 5987 extended value (`UTF-8''t%C3%BCr.wav`), percent-decoded.
/// `None` for another charset or bytes that are not UTF-8.
fn extended_value(value: &str) -> Option<String> {
    let (charset, rest) = value.split_once('\'')?;
    let (_language, encoded) = rest.split_once('\'')?;
    if !charset.eq_ignore_ascii_case("utf-8") {
        return None;
    }
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded)
        .ok()
        .filter(|name| !name.is_empty())
}

/// A filename for a file part sent without one, with the extension its
/// content type implies (engines often guess the format from the name).
fn default_filename(content_type: Option<&str>) -> String {
    let essence = content_type
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().to_ascii_lowercase());
    let extension = match essence.as_deref() {
        Some("audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave") => "wav",
        Some("audio/mpeg" | "audio/mp3") => "mp3",
        Some("audio/mp4" | "audio/m4a" | "audio/x-m4a") => "m4a",
        Some("audio/aac") => "aac",
        Some("audio/ogg") => "ogg",
        Some("audio/opus") => "opus",
        Some("audio/flac" | "audio/x-flac") => "flac",
        Some("audio/webm" | "video/webm") => "webm",
        Some("video/mp4") => "mp4",
        _ => "bin",
    };
    format!("audio.{extension}")
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

    #[test]
    fn probe_a_filename_with_a_semicolon_survives() {
        let body = Bytes::from_static(
            b"--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"call; part 1.m4a\"\r\n\
Content-Type: audio/mp4\r\n\r\nDATA\r\n--B--\r\n",
        );
        let parts = parse_multipart(&body, "B").unwrap();
        assert_eq!(parts[0].name, "file");
        assert_eq!(parts[0].filename.as_deref(), Some("call; part 1.m4a"));
        assert_eq!(parts[0].content_type.as_deref(), Some("audio/mp4"));
    }

    #[test]
    fn probe_a_filename_star_only_part_is_still_a_file() {
        let body = Bytes::from_static(
            b"--B\r\nContent-Disposition: form-data; name=\"file\"; filename*=UTF-8''t%C3%BCr.wav\r\n\r\nDATA\r\n--B--\r\n",
        );
        let parts = parse_multipart(&body, "B").unwrap();
        assert_eq!(parts[0].filename.as_deref(), Some("t\u{fc}r.wav"));
    }

    #[test]
    fn filename_star_wins_and_a_bad_one_falls_back_to_filename() {
        let params = disposition_params(
            "form-data; name=\"file\"; filename=\"plain.wav\"; filename*=utf-8'en'%E2%82%AC.wav",
        );
        assert_eq!(
            params,
            vec![
                ("name".to_string(), "file".to_string()),
                ("filename".to_string(), "plain.wav".to_string()),
                (
                    "filename*".to_string(),
                    "utf-8'en'%E2%82%AC.wav".to_string()
                ),
            ]
        );
        assert_eq!(
            extended_value("utf-8'en'%E2%82%AC.wav").as_deref(),
            Some("\u{20ac}.wav")
        );
        assert_eq!(extended_value("iso-8859-1''a.wav"), None);
        assert_eq!(extended_value("UTF-8''%FF.wav"), None, "not UTF-8");
        let body = Bytes::from_static(
            b"--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"plain.wav\"; filename*=latin1''x.wav\r\n\r\nDATA\r\n--B--\r\n",
        );
        let parts = parse_multipart(&body, "B").unwrap();
        assert_eq!(parts[0].filename.as_deref(), Some("plain.wav"));
    }

    #[test]
    fn quoted_values_keep_escaped_quotes_and_unquoted_values_are_trimmed() {
        let params = disposition_params(r#"form-data;name=model ; filename="say \"hi\"; ok.wav" "#);
        assert_eq!(params[0], ("name".to_string(), "model".to_string()));
        assert_eq!(
            params[1],
            ("filename".to_string(), "say \"hi\"; ok.wav".to_string())
        );
    }

    #[test]
    fn a_file_part_without_a_filename_is_forwarded_as_a_file() {
        let body = Bytes::from_static(
            b"--B\r\nContent-Disposition: form-data; name=\"file\"\r\n\
Content-Type: audio/x-m4a\r\n\r\nDATA\r\n\
--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm\r\n--B--\r\n",
        );
        let parts = parse_multipart(&body, "B").unwrap();
        assert_eq!(parts[0].filename.as_deref(), Some("audio.m4a"));
        assert_eq!(parts[1].filename, None, "a text field stays a field");
        assert_eq!(default_filename(None), "audio.bin");
        assert_eq!(default_filename(Some("audio/wav; codecs=1")), "audio.wav");
        assert_eq!(
            default_filename(Some("application/octet-stream")),
            "audio.bin"
        );
    }
}
