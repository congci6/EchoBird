//! The auto-degrade, driven over real HTTP.
//!
//! A provider that answers `500 not implemented` on `/v1/responses` and serves
//! Chat Completions is the exact case. A Responses client must get a real
//! answer — and the second request must not pay the doomed round trip again.

use axum::{extract::State, routing::post, Json, Router};
use echobird_lib::services::protocol::WireProtocol;
use echobird_lib::services::protocol_bridge::{
    build_router, refuses_responses, remember_no_responses, set_target, BridgeTarget,
};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// `set_target` writes one process-wide target, so the two cases below cannot
/// run at the same time without the second hijacking the first one's provider.
static TARGET_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);

/// Serves Chat Completions. Refuses `/v1/responses` with the wording a real
/// OpenAI-compatible gateway uses, and counts how often it was asked.
async fn provider(
    State(seen): State<Seen>,
    uri: axum::http::Uri,
    Json(_body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    let path = uri.path().to_string();
    seen.0.lock().unwrap().push(path.clone());
    if path.contains("responses") {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": {"code": 500, "message": "not implemented", "type": "invalid_request_error"}
            })),
        );
    }
    (
        axum::http::StatusCode::OK,
        Json(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 0,
            "model": "upstream",
            "choices": [{"index": 0,
                "message": {"role": "assistant", "content": "pong"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        })),
    )
}

async fn serve_bridge() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn send_responses(url: &str) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(url)
        .json(&json!({"model": "client-model", "input": "ping", "max_output_tokens": 64}))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let text = r.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

#[tokio::test]
async fn a_responses_client_is_answered_and_the_verdict_is_remembered() {
    let _guard = TARGET_LOCK.lock().await;
    let seen = Seen::default();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let provider_addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", post(provider))
        .route("/v1/chat/completions", post(provider))
        .with_state(seen.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let base = format!("http://{provider_addr}/v1");
    set_target(BridgeTarget {
        base_url: base.clone(),
        api_key: "k".to_string(),
        model: "upstream".to_string(),
        protocol: WireProtocol::OpenaiResponses,
        auto_degrade: true,
    });
    let bridge = serve_bridge().await;
    let url = format!("http://{bridge}/v1/responses");

    assert!(!refuses_responses(&base), "nothing is known up front");

    // First request: the provider refuses, the bridge degrades and retries, and
    // the client still gets an answer in its own dialect.
    let (status, body) = send_responses(&url).await;
    assert_eq!(status, 200, "the client must be answered, not 500: {body}");
    assert_eq!(body["object"], "response", "{body}");
    assert_eq!(body["output"][0]["content"][0]["text"], "pong", "{body}");
    // The client's own model id comes back, as it always does.
    assert_eq!(body["model"], "client-model", "{body}");

    assert!(
        refuses_responses(&base),
        "the verdict must be remembered for the rest of the session"
    );

    let after_first = seen.0.lock().unwrap().clone();
    assert_eq!(
        after_first,
        ["/v1/responses", "/v1/chat/completions"],
        "one refusal, then one degraded retry"
    );

    // Second request: the refusal is already known, so the doomed round trip
    // is skipped entirely and only the working path is used.
    let (status, body) = send_responses(&url).await;
    assert_eq!(status, 200, "{body}");
    let after_second = seen.0.lock().unwrap().clone();
    assert_eq!(
        after_second.len(),
        3,
        "the second request must not re-ask: {after_second:?}"
    );
    assert_eq!(
        after_second[2], "/v1/chat/completions",
        "second request goes straight to the working dialect: {after_second:?}"
    );

    // Trailing-slash and /v1 differences must not produce a second entry for
    // one provider.
    remember_no_responses("http://other.example/v1/");
    assert!(refuses_responses("http://other.example/v1"));
    assert!(refuses_responses("http://other.example/v1/"));
}

/// The switch is off by default, and off must mean off.
///
/// A provider that refuses Responses produces the same 500 it produced before
/// this feature existed, the failure is handed to the user verbatim, and — the
/// part that matters — nothing is learned. Learning while switched off would
/// mean the user's answer to "should I degrade silently?" had been ignored
/// until the second request.
#[tokio::test]
async fn with_the_switch_off_the_refusal_is_forwarded_and_nothing_is_learned() {
    let _guard = TARGET_LOCK.lock().await;
    let seen = Seen::default();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let provider_addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", post(provider))
        .route("/v1/chat/completions", post(provider))
        .with_state(seen.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let base = format!("http://{provider_addr}/v1");
    set_target(BridgeTarget {
        base_url: base.clone(),
        api_key: "k".to_string(),
        model: "upstream".to_string(),
        protocol: WireProtocol::OpenaiResponses,
        auto_degrade: false,
    });
    let bridge = serve_bridge().await;
    let url = format!("http://{bridge}/v1/responses");

    for attempt in 1..=2 {
        let (status, body) = send_responses(&url).await;
        assert_eq!(
            status, 500,
            "attempt {attempt}: the refusal is the user's to act on, not ours to hide: {body}"
        );
    }

    assert_eq!(
        seen.0.lock().unwrap().as_slice(),
        ["/v1/responses", "/v1/responses"],
        "each attempt asks upstream once and never degrades"
    );
    assert!(
        !refuses_responses(&base),
        "switched off must not learn anything about the provider"
    );
}
