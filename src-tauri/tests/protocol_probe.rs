//! Probing which dialects a provider actually serves.
//!
//! A user standing at "add a model" has a base URL and a key and no way to know
//! which of the four dialects will answer. Today the only way to find out is to
//! wire the model to a tool and read the error. These tests pin the contract of
//! the probe that answers it up front.
//!
//! The interesting case is a provider that serves some dialects and not others,
//! which is the norm for OpenAI-compatible gateways: `vsllm.cc` answers
//! `not implemented` on `/v1/responses` while serving the other three.

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use echobird_lib::services::protocol::WireProtocol;
use echobird_lib::services::protocol_probe::{probe_all, DialectReport, Outcome};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);

/// Serves Chat Completions and Messages; refuses Responses and Gemini the way a
/// real gateway does — with a 5xx and an explicit "not implemented".
async fn partial_provider(
    State(seen): State<Seen>,
    uri: axum::http::Uri,
    Json(_body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let path = uri.path().to_string();
    seen.0.lock().unwrap().push(path.clone());

    if path.contains("responses") || path.contains("generateContent") {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"code": 500, "message": "not implemented"}})),
        );
    }
    if path.contains("messages") {
        return (
            StatusCode::OK,
            Json(json!({
                "id": "msg_1", "type": "message", "role": "assistant",
                "model": "m", "content": [{"type": "text", "text": "pong"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 2}
            })),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "model": "m",
            "choices": [{"index": 0,
                "message": {"role": "assistant", "content": "pong"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        })),
    )
}

/// A provider that rejects the key on every path.
async fn unauthorized() -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": {"message": "invalid api key"}})),
    )
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn partial(seen: Seen) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(partial_provider))
        .route("/v1/messages", post(partial_provider))
        .route("/v1/responses", post(partial_provider))
        .route("/v1beta/models/{model}", post(partial_provider))
        .route(
            "/v1beta/models/{model}/streamGenerateContent",
            post(partial_provider),
        )
        .with_state(seen)
}

fn report(reports: &[DialectReport], protocol: WireProtocol) -> &DialectReport {
    reports
        .iter()
        .find(|r| r.protocol == protocol.as_str())
        .unwrap_or_else(|| panic!("{protocol:?} missing from {reports:?}"))
}

/// A two-dialect provider is reported as exactly two available and two not,
/// and the unavailable ones name the reason — that is the whole point of the
/// button, since the user cannot otherwise tell which protocol to pick.
#[tokio::test]
async fn a_probe_reports_which_dialects_answer_and_which_do_not() {
    let seen = Seen::default();
    let addr = serve(partial(seen.clone())).await;

    let reports = probe_all(&format!("http://{addr}/v1"), "test-key", "m").await;

    assert!(report(&reports, WireProtocol::OpenaiChat).available);
    assert!(report(&reports, WireProtocol::AnthropicMessages).available);
    assert!(!report(&reports, WireProtocol::OpenaiResponses).available);
    assert!(!report(&reports, WireProtocol::GeminiGenerateContent).available);

    // The unavailable one must carry the provider's own wording, or the report
    // is not actionable — "unavailable" alone does not tell the user what to
    // do about it.
    let responses = report(&reports, WireProtocol::OpenaiResponses);
    assert_eq!(responses.outcome, Outcome::Unsupported);
    assert!(
        responses
            .detail
            .as_deref()
            .unwrap_or_default()
            .to_lowercase()
            .contains("not implemented"),
        "the provider's wording is the useful part: {:?}",
        responses.detail
    );
}

/// A 401 is a wrong key, not a missing endpoint. Reporting every dialect as
/// unavailable would send the user off to re-pick a protocol when the actual
/// problem is the credential.
#[tokio::test]
async fn a_provider_error_is_distinguished_from_a_missing_dialect() {
    let addr = serve(Router::new().fallback(post(unauthorized))).await;

    let reports = probe_all(&format!("http://{addr}/v1"), "bad-key", "m").await;
    for entry in &reports {
        assert!(
            !entry.available,
            "{} must not be reported available",
            entry.protocol
        );
        assert_eq!(
            entry.outcome,
            Outcome::Auth,
            "{} should be an auth outcome, got {:?}",
            entry.protocol,
            entry.outcome
        );
    }
}

/// Every dialect is probed, always: the user needs the full picture to choose,
/// and stopping at the first failure would report "nothing works" for a
/// provider that serves three of the four.
#[tokio::test]
async fn all_four_dialects_are_probed_even_when_one_fails() {
    let seen = Seen::default();
    let addr = serve(partial(seen.clone())).await;

    let reports = probe_all(&format!("http://{addr}/v1"), "k", "m").await;
    assert_eq!(reports.len(), 4, "one report per dialect: {reports:?}");

    let mut paths = seen.0.lock().unwrap().clone();
    paths.sort();
    paths.dedup();
    assert!(
        paths.len() >= 2,
        "several endpoints were actually called: {paths:?}"
    );
}
