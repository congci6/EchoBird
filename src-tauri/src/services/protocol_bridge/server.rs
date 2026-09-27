//! HTTP surface of the protocol bridge.
//!
//! Every route accepts the dialect the client speaks, converts it to the
//! provider's dialect, and converts the answer back — streaming included, so a
//! tool that needs incremental output still gets it.

use super::BridgeTarget;
use crate::services::protocol::{self, StreamDecoder, StreamEncoder, WireProtocol};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

/// Requests carry full conversation histories plus base64 images; 64 MiB is
/// generous but keeps a runaway client from exhausting memory.
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct BridgeState {
    pub(crate) http_client: reqwest::Client,
}

impl BridgeState {
    fn build() -> Result<Self, String> {
        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            // Long-lived SSE responses must not be cut off mid-stream.
            .read_timeout(Duration::from_secs(600))
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("reqwest client build failed: {e}"))?;
        Ok(Self { http_client })
    }
}

/// Build the bridge router. All four dialects share one port so a client only
/// ever needs a base URL.
pub fn build_router() -> Result<Router, String> {
    use axum::extract::DefaultBodyLimit;
    Ok(Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .route("/v1/messages", post(messages))
        .route("/messages", post(messages))
        // Gemini embeds the model and the method in the path
        // (`models/gemini-2.5-pro:generateContent`), so one route covers
        // generateContent, streamGenerateContent and countTokens.
        .route("/v1beta/models/{model_action}", post(gemini))
        .route("/v1/models/{model_action}", post(gemini))
        .route("/v1beta/models", post(gemini_collection))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(BridgeState::build()?))
}

// ─── Route handlers ───

async fn chat_completions(State(state): State<BridgeState>, Json(body): Json<Value>) -> Response {
    bridge(state, WireProtocol::OpenaiChat, None, body).await
}

async fn responses(State(state): State<BridgeState>, Json(body): Json<Value>) -> Response {
    bridge(state, WireProtocol::OpenaiResponses, None, body).await
}

async fn messages(State(state): State<BridgeState>, Json(body): Json<Value>) -> Response {
    bridge(state, WireProtocol::AnthropicMessages, None, body).await
}

async fn gemini(
    State(state): State<BridgeState>,
    Path(model_action): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let (model, method) = split_gemini_path(&model_action);
    match method.as_str() {
        "countTokens" | "count_tokens" => {
            // Token counting is advisory; answer locally from the body EchoBird
            // already parsed rather than forwarding a method we cannot convert.
            count_tokens_locally(&body)
        }
        _ => {
            bridge(
                state,
                WireProtocol::GeminiGenerateContent,
                Some(model),
                body,
            )
            .await
        }
    }
}

async fn gemini_collection(State(state): State<BridgeState>, Json(body): Json<Value>) -> Response {
    bridge(state, WireProtocol::GeminiGenerateContent, None, body).await
}

/// Split `gemini-2.5-pro:generateContent` into its model id and method.
fn split_gemini_path(raw: &str) -> (String, String) {
    match raw.rsplit_once(':') {
        Some((model, method)) => (model.to_string(), method.to_string()),
        None => (raw.to_string(), "generateContent".to_string()),
    }
}

// ─── Core conversion ───

/// Convert a request in `client_protocol` into `target.protocol`, forward it,
/// and render the answer back in `client_protocol`.
async fn bridge(
    state: BridgeState,
    client_protocol: WireProtocol,
    path_model: Option<String>,
    body: Value,
) -> Response {
    let Some(target) = super::target() else {
        return protocol_error(
            client_protocol,
            StatusCode::SERVICE_UNAVAILABLE,
            "EchoBird's protocol bridge has no provider configured",
        );
    };

    let request = match protocol::parse_request(client_protocol, &body, path_model.as_deref()) {
        Ok(request) => request,
        Err(message) => {
            return protocol_error(client_protocol, StatusCode::BAD_REQUEST, &message);
        }
    };

    // The bridge serves one model; a client that named its own is taken at its
    // word, otherwise the configured model applies.
    let mut request = request;
    if request.model.trim().is_empty() {
        request.model = target.model.clone();
    }
    let client_model = request.model.clone();

    let upstream_body = protocol::encode_request(target.protocol, &request);
    let streaming = request.stream;
    let url = upstream_url(&target, streaming);

    let mut outgoing = state
        .http_client
        .post(&url)
        .json(&upstream_body)
        .header(header::CONTENT_TYPE, "application/json");
    outgoing = apply_auth(outgoing, target.protocol, &target.api_key);

    let response = match outgoing.send().await {
        Ok(response) => response,
        Err(error) => {
            return protocol_error(
                client_protocol,
                StatusCode::BAD_GATEWAY,
                &format!("upstream request failed: {error}"),
            );
        }
    };

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return forward_error(client_protocol, status, &text);
    }

    if streaming {
        return stream_through(response, target.protocol, client_protocol, client_model);
    }

    let payload: Value = match response.json().await {
        Ok(payload) => payload,
        Err(error) => {
            return protocol_error(
                client_protocol,
                StatusCode::BAD_GATEWAY,
                &format!("upstream returned a non-JSON body: {error}"),
            );
        }
    };
    let canonical = protocol::parse_response(target.protocol, &payload);
    let rendered = protocol::encode_response(client_protocol, &canonical, &client_model);
    (StatusCode::OK, axum::Json(rendered)).into_response()
}

/// Relay an upstream SSE stream, re-encoding it into the client's dialect.
fn stream_through(
    response: reqwest::Response,
    upstream: WireProtocol,
    client: WireProtocol,
    model: String,
) -> Response {
    // The spawned task outlives this call, so the model id must be owned.
    let mut source = response.bytes_stream();
    let (sender, receiver) =
        tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(32);

    tokio::spawn(async move {
        let mut decoder = StreamDecoder::new(upstream);
        let mut encoder = StreamEncoder::new(client, &model);
        let preamble = encoder.start();
        if !preamble.is_empty()
            && sender
                .send(Ok(axum::body::Bytes::from(preamble)))
                .await
                .is_err()
        {
            return;
        }
        while let Some(item) = source.next().await {
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(error) => {
                    let _ = sender.send(Err(std::io::Error::other(error))).await;
                    return;
                }
            };
            for event in decoder.push(&chunk) {
                let bytes = encoder.push(&event);
                if !bytes.is_empty()
                    && sender
                        .send(Ok(axum::body::Bytes::from(bytes)))
                        .await
                        .is_err()
                {
                    return;
                }
            }
        }
        for event in decoder.finish() {
            let bytes = encoder.push(&event);
            if !bytes.is_empty()
                && sender
                    .send(Ok(axum::body::Bytes::from(bytes)))
                    .await
                    .is_err()
            {
                return;
            }
        }
        let trailer = encoder.finish();
        if !trailer.is_empty() {
            let _ = sender.send(Ok(axum::body::Bytes::from(trailer))).await;
        }
    });

    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    sse_response(stream)
}

/// An SSE response with the headers every client checks before parsing.
fn sse_response<S>(stream: S) -> Response
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, std::io::Error>> + Send + 'static,
{
    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        "text/event-stream".parse().expect("static header value"),
    );
    headers.insert("cache-control", "no-cache".parse().expect("static header"));
    headers.insert("x-accel-buffering", "no".parse().expect("static header"));
    response
}

// ─── Upstream addressing ───

/// Build the upstream URL for a target.
///
/// The shapes users actually paste into a model form differ per protocol, so
/// each one is normalized the same way `llm_client` and `smart_router` already
/// do it, rather than blindly appending a path.
pub(crate) fn upstream_url(target: &BridgeTarget, streaming: bool) -> String {
    let base = target.base_url.trim_end_matches('/');
    match target.protocol {
        WireProtocol::OpenaiChat => {
            if base.ends_with("/chat/completions") {
                base.to_string()
            } else {
                format!("{base}/chat/completions")
            }
        }
        WireProtocol::OpenaiResponses => {
            if base.ends_with("/responses") {
                base.to_string()
            } else {
                format!("{base}/responses")
            }
        }
        WireProtocol::AnthropicMessages => anthropic_url(base),
        WireProtocol::GeminiGenerateContent => {
            let method = if streaming {
                "streamGenerateContent"
            } else {
                "generateContent"
            };
            if base.contains(":generateContent") || base.contains(":streamGenerateContent") {
                base.to_string()
            } else {
                // Gemini selects the streaming variant with a query parameter.
                let suffix = if streaming { "?alt=sse" } else { "" };
                format!("{base}/models/{}:{method}{suffix}", target.model)
            }
        }
    }
}

/// Resolve an `anthropic_url` to the full `/v1/messages` endpoint.
///
/// Four shapes occur in practice, and getting this wrong is the usual reason a
/// user pastes a working URL and still sees a 404:
///   1. already ends in `/messages` — use as-is
///   2. local proxy `/anthropic` — append `/messages` (the local proxy strips
///      `/v1` itself; remote vendors do not)
///   3. already ends in `/v1` — append `/messages`
///   4. bare host or remote `/anthropic` prefix — append `/v1/messages`
fn anthropic_url(base: &str) -> String {
    if base.contains("/messages") {
        return base.to_string();
    }
    let is_local = base.contains("127.0.0.1") || base.contains("localhost");
    if (base.ends_with("/anthropic") && is_local) || base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// Attach credentials in the dialect the provider expects.
fn apply_auth(
    request: reqwest::RequestBuilder,
    protocol: WireProtocol,
    api_key: &str,
) -> reqwest::RequestBuilder {
    let key = api_key.trim();
    if key.is_empty() {
        return request;
    }
    match protocol.auth_style() {
        protocol::AuthStyle::Bearer => {
            request.header(header::AUTHORIZATION, format!("Bearer {key}"))
        }
        protocol::AuthStyle::AnthropicKey => request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
        protocol::AuthStyle::GoogleKey => request.header("x-goog-api-key", key),
    }
}

// ─── Errors ───

/// Render an error in the caller's dialect, so a client never has to parse a
/// foreign error shape.
fn protocol_error(protocol: WireProtocol, status: StatusCode, message: &str) -> Response {
    let code = status.as_u16();
    let body = match protocol {
        WireProtocol::AnthropicMessages => json!({
            "type": "error",
            "error": {"type": anthropic_error_type(status), "message": message},
        }),
        WireProtocol::GeminiGenerateContent => json!({
            "error": {
                "code": code,
                "message": message,
                "status": gemini_error_status(status),
            }
        }),
        // Chat Completions and Responses share the OpenAI error envelope.
        WireProtocol::OpenaiChat | WireProtocol::OpenaiResponses => json!({
            "error": {"message": message, "type": "invalid_request_error", "code": code}
        }),
    };
    (status, Json(body)).into_response()
}

fn anthropic_error_type(status: StatusCode) -> &'static str {
    if status.is_client_error() {
        "invalid_request_error"
    } else {
        "api_error"
    }
}

fn gemini_error_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "INVALID_ARGUMENT",
        StatusCode::UNAUTHORIZED => "UNAUTHENTICATED",
        StatusCode::FORBIDDEN => "PERMISSION_DENIED",
        StatusCode::NOT_FOUND => "NOT_FOUND",
        StatusCode::TOO_MANY_REQUESTS => "RESOURCE_EXHAUSTED",
        _ => "INTERNAL",
    }
}

/// Relay an upstream failure, preferring the provider's own message so the user
/// sees the real cause (bad key, unknown model, quota) instead of a generic one.
fn forward_error(protocol: WireProtocol, status: StatusCode, body: &str) -> Response {
    let message =
        extract_upstream_message(body).unwrap_or_else(|| format!("upstream returned {status}"));
    let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    protocol_error(protocol, status, &message)
}

/// Pull a human-readable message out of whichever error envelope the provider
/// used. Falls back to a truncated body so nothing is silently swallowed.
fn extract_upstream_message(body: &str) -> Option<String> {
    if body.trim().is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let candidate = value
            .get("error")
            .and_then(|error| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| error.as_str())
            })
            .or_else(|| value.get("message").and_then(Value::as_str));
        if let Some(message) = candidate {
            if !message.is_empty() {
                return Some(message.to_string());
            }
        }
    }
    let trimmed = body.trim();
    let excerpt: String = trimmed.chars().take(300).collect();
    Some(excerpt)
}

/// Answer Gemini's `countTokens` locally.
///
/// Counting is advisory, and the method has no counterpart in the other three
/// dialects, so a rough estimate from the parsed request beats a 404 — the
/// alternative is forwarding a call we cannot convert in either direction.
fn count_tokens_locally(body: &Value) -> Response {
    let request = protocol::parse_request(WireProtocol::GeminiGenerateContent, body, None)
        .unwrap_or_default();
    // ~4 characters per token is the usual English approximation.
    let characters: usize = request
        .system
        .iter()
        .chain(
            request
                .messages
                .iter()
                .flat_map(|message| message.content.iter()),
        )
        .filter_map(|block| match block {
            protocol::ContentBlock::Text { text } => Some(text.len()),
            _ => None,
        })
        .sum();
    let total = (characters / 4) as u64;
    (
        StatusCode::OK,
        Json(json!({
            "totalTokens": total,
            "estimated": true,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(protocol: WireProtocol, base: &str) -> BridgeTarget {
        BridgeTarget {
            base_url: base.to_string(),
            api_key: "k".to_string(),
            model: "m".to_string(),
            protocol,
        }
    }

    #[test]
    fn every_protocol_builds_its_documented_endpoint() {
        let cases = [
            (
                WireProtocol::OpenaiChat,
                "https://api.example/v1",
                "https://api.example/v1/chat/completions",
            ),
            (
                WireProtocol::OpenaiResponses,
                "https://api.example/v1",
                "https://api.example/v1/responses",
            ),
            (
                WireProtocol::AnthropicMessages,
                "https://api.anthropic.com",
                "https://api.anthropic.com/v1/messages",
            ),
            (
                WireProtocol::GeminiGenerateContent,
                "https://generativelanguage.googleapis.com/v1beta",
                "https://generativelanguage.googleapis.com/v1beta/models/m:generateContent",
            ),
        ];
        for (protocol, base, expected) in cases {
            assert_eq!(
                upstream_url(&target(protocol, base), false),
                expected,
                "{protocol}"
            );
        }
    }

    #[test]
    fn gemini_streaming_uses_the_sse_variant() {
        let streaming = upstream_url(
            &target(
                WireProtocol::GeminiGenerateContent,
                "https://generativelanguage.googleapis.com/v1beta",
            ),
            true,
        );
        assert!(
            streaming.ends_with(":streamGenerateContent?alt=sse"),
            "{streaming}"
        );
    }

    #[test]
    fn endpoints_are_not_appended_twice() {
        // A user pasting the full endpoint must get it back unchanged.
        for (protocol, base) in [
            (
                WireProtocol::OpenaiChat,
                "https://api.example/v1/chat/completions",
            ),
            (
                WireProtocol::OpenaiResponses,
                "https://api.example/v1/responses",
            ),
        ] {
            assert_eq!(upstream_url(&target(protocol, base), false), base);
        }
        let full_gemini = "https://g.example/v1beta/models/m:generateContent";
        assert_eq!(
            upstream_url(
                &target(WireProtocol::GeminiGenerateContent, full_gemini),
                false
            ),
            full_gemini
        );
    }

    #[test]
    fn anthropic_url_handles_every_pasted_shape() {
        assert_eq!(
            anthropic_url("https://api.anthropic.com/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            anthropic_url("https://api.example/v1"),
            "https://api.example/v1/messages"
        );
        assert_eq!(
            anthropic_url("https://api.deepseek.com/anthropic"),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        // The local proxy strips /v1 itself, so it takes the short form.
        assert_eq!(
            anthropic_url("http://127.0.0.1:1234/anthropic"),
            "http://127.0.0.1:1234/anthropic/messages"
        );
    }

    #[test]
    fn gemini_paths_split_into_model_and_method() {
        assert_eq!(
            split_gemini_path("gemini-2.5-pro:generateContent"),
            ("gemini-2.5-pro".to_string(), "generateContent".to_string())
        );
        assert_eq!(
            split_gemini_path("gemini-2.5-pro:streamGenerateContent"),
            (
                "gemini-2.5-pro".to_string(),
                "streamGenerateContent".to_string()
            )
        );
        // No method suffix still resolves to the default.
        assert_eq!(
            split_gemini_path("gemini-2.5-pro").1,
            "generateContent".to_string()
        );
    }

    #[test]
    fn upstream_error_messages_are_unwrapped() {
        assert_eq!(
            extract_upstream_message(r#"{"error":{"message":"bad key"}}"#).as_deref(),
            Some("bad key")
        );
        assert_eq!(
            extract_upstream_message(r#"{"error":"quota exceeded"}"#).as_deref(),
            Some("quota exceeded")
        );
        assert_eq!(
            extract_upstream_message(r#"{"message":"unknown model"}"#).as_deref(),
            Some("unknown model")
        );
        assert_eq!(extract_upstream_message(""), None);
        // A non-JSON body still yields something the user can act on.
        let html = extract_upstream_message("<html>502</html>").expect("falls back");
        assert!(html.contains("502"), "{html}");
    }

    #[test]
    fn errors_are_rendered_in_the_callers_dialect() {
        let anthropic = protocol_error(
            WireProtocol::AnthropicMessages,
            StatusCode::BAD_REQUEST,
            "nope",
        );
        assert_eq!(anthropic.status(), StatusCode::BAD_REQUEST);

        let gemini = protocol_error(
            WireProtocol::GeminiGenerateContent,
            StatusCode::TOO_MANY_REQUESTS,
            "slow",
        );
        assert_eq!(gemini.status(), StatusCode::TOO_MANY_REQUESTS);

        let chat = protocol_error(WireProtocol::OpenaiChat, StatusCode::BAD_REQUEST, "nope");
        assert_eq!(chat.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn count_tokens_answers_locally_instead_of_404ing() {
        let body = json!({
            "contents": [{"role": "user", "parts": [{"text": "hello there friend"}]}]
        });
        let response = count_tokens_locally(&body);
        assert_eq!(response.status(), StatusCode::OK);
    }
}
