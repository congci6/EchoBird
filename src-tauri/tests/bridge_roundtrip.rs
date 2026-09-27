//! End-to-end proof of the 503 fix, over real HTTP.
//!
//! A provider that serves ONLY `/v1/chat/completions` stands in for the
//! supplier behind the reported failure. A Codex-shaped Responses request is
//! sent to the bridge's `/v1/responses`; the provider must receive
//! `/v1/chat/completions` carrying the REAL model id, and the answer must come
//! back in Responses shape.

use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use echobird_lib::services::protocol::WireProtocol;
use echobird_lib::services::protocol_bridge::{build_router, set_target, BridgeTarget};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// `set_target` writes one process-wide bridge target, so two round trips
/// cannot run at the same time without the second one hijacking the first
/// one's provider. Tests within a binary do run in parallel by default.
static TARGET_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Default)]
struct Seen {
    paths: Arc<Mutex<Vec<String>>>,
    models: Arc<Mutex<Vec<String>>>,
    forwards: Arc<Mutex<Value>>,
}

async fn upstream_only_chat(
    State(seen): State<Seen>,
    uri: axum::http::Uri,
    Json(body): Json<Value>,
) -> Json<Value> {
    seen.paths.lock().unwrap().push(uri.path().to_string());
    seen.models.lock().unwrap().push(
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    );
    Json(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": "space-bunny-free",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "pong"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}
    }))
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A provider that speaks Responses natively, and records the fields the
/// canonical form has no room for so the test can prove they were not dropped.
async fn upstream_only_responses(
    State(seen): State<Seen>,
    uri: axum::http::Uri,
    Json(body): Json<Value>,
) -> Json<Value> {
    seen.paths.lock().unwrap().push(uri.path().to_string());
    seen.models.lock().unwrap().push(
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    );
    *seen.forwards.lock().unwrap() = json!({
        "previous_response_id": body.get("previous_response_id").cloned().unwrap_or(Value::Null),
        "store": body.get("store").cloned().unwrap_or(Value::Null),
        "include": body.get("include").cloned().unwrap_or(Value::Null),
    });
    Json(json!({
        "id": "resp_1",
        "object": "response",
        "created_at": 0,
        "model": "space-bunny-free",
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "pong", "annotations": []}]
        }],
        "usage": {"input_tokens": 7, "output_tokens": 2, "total_tokens": 9}
    }))
}

/// The same provider, answering with SSE. The payload carries an unknown event
/// type and an unknown top-level field, both of which the canonical round trip
/// would flatten away.
async fn upstream_only_responses_stream(
    State(seen): State<Seen>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    seen.models.lock().unwrap().push(
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    );
    let sse = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":1,\"response\":{\"id\":\"resp_1\",\"model\":\"space-bunny-free\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"sequence_number\":7,\"delta\":\"pong\",\"surprise_field\":{\"keep\":true}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":9,\"response\":{\"id\":\"resp_1\",\"model\":\"space-bunny-free\",\"status\":\"completed\"}}\n\n",
        "data: [DONE]\n\n",
    );
    (
        axum::http::StatusCode::OK,
        [("content-type", "text/event-stream")],
        sse,
    )
        .into_response()
}

#[tokio::test]
async fn codex_responses_request_reaches_a_chat_only_provider() {
    let _guard = TARGET_LOCK.lock().await;
    let seen = Seen::default();
    // A provider that only implements Chat Completions. Anything else 404s.
    let provider = serve(
        Router::new()
            .route("/v1/chat/completions", post(upstream_only_chat))
            .with_state(seen.clone()),
    )
    .await;

    set_target(BridgeTarget {
        // Production base_url carries the /v1 prefix; the bridge appends the endpoint.
        base_url: format!("http://{provider}/v1"),
        api_key: "test-key".to_string(),
        model: "space-bunny-free".to_string(),
        protocol: WireProtocol::OpenaiChat,
    });
    let bridge = serve(build_router().unwrap()).await;

    // Exactly the shape Codex sends — including the model. ChatGPT Desktop
    // persists the last model it used in its own state and sends that instead
    // of the `model` in config.toml, so the display label arrives on the wire
    // even though EchoBird configured the real id. The bridge must not take a
    // client at its word; that label is what produced the 503.
    let response = reqwest::Client::new()
        .post(format!("http://{bridge}/v1/responses"))
        .json(&json!({
            "model": "gpt-5.5",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "ping"}]}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        200,
        "bridge refused the Responses request"
    );
    let body: Value = response.json().await.unwrap();

    let paths = seen.paths.lock().unwrap().clone();
    let models = seen.models.lock().unwrap().clone();
    println!("provider received: {paths:?} models={models:?}");
    println!(
        "bridge answered:  {}",
        serde_json::to_string(&body).unwrap()
    );

    assert_eq!(
        paths,
        vec!["/v1/chat/completions".to_string()],
        "the provider must be called on Chat Completions, not /v1/responses"
    );
    assert_eq!(
        models,
        vec!["space-bunny-free".to_string()],
        "the provider must receive the real model id, never the gpt-5.5 label"
    );
    assert!(!models.iter().any(|m| m.contains("gpt-5.5")));

    assert_eq!(body["object"], "response", "answer is not Responses-shaped");
    assert_eq!(
        body["model"], "gpt-5.5",
        "the client must still see the model it asked for"
    );
    let text = body["output"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert_eq!(text, "pong", "answer text did not survive the round trip");
    assert_eq!(body["usage"]["total_tokens"], 9);
}

/// The provider serves Responses natively, so the user picks
/// `openai-responses` and nothing needs translating. The hop still has to
/// exist — it is what pins the model id — and it must not translate on the
/// way through, because the canonical form has no room for
/// `previous_response_id`, `store`, `include` or replayed reasoning items.
/// Dropping any of them is what breaks Codex on a Responses-native provider.
#[tokio::test]
async fn responses_client_reaches_a_responses_provider_without_being_flattened() {
    let _guard = TARGET_LOCK.lock().await;
    let seen = Seen::default();

    let provider = serve(
        Router::new()
            .route("/v1/responses", post(upstream_only_responses))
            .with_state(seen.clone()),
    )
    .await;

    set_target(BridgeTarget {
        base_url: format!("http://{provider}/v1"),
        api_key: "test-key".to_string(),
        model: "space-bunny-free".to_string(),
        protocol: WireProtocol::OpenaiResponses,
    });
    let bridge = serve(build_router().unwrap()).await;

    let response = reqwest::Client::new()
        .post(format!("http://{bridge}/v1/responses"))
        .json(&json!({
            "model": "gpt-5.5",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "ping"}]}],
            "previous_response_id": "resp_prev_1",
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        200,
        "bridge refused the Responses request"
    );
    let body: Value = response.json().await.unwrap();

    let paths = seen.paths.lock().unwrap().clone();
    let models = seen.models.lock().unwrap().clone();
    let forwards = seen.forwards.lock().unwrap().clone();
    println!("provider received: {paths:?} models={models:?}");

    assert_eq!(
        paths,
        vec!["/v1/responses".to_string()],
        "a Responses provider must be called on /v1/responses"
    );
    assert_eq!(
        models,
        vec!["space-bunny-free".to_string()],
        "the provider must receive the real model id, never the gpt-5.5 label"
    );

    // Everything the canonical form cannot represent has to arrive intact.
    assert_eq!(forwards["previous_response_id"], json!("resp_prev_1"));
    assert_eq!(forwards["store"], json!(false));
    assert_eq!(forwards["include"], json!(["reasoning.encrypted_content"]));

    assert_eq!(
        body["model"], "gpt-5.5",
        "the client must still see the model it asked for"
    );
    let text = body["output"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert_eq!(text, "pong", "answer text did not survive the passthrough");
}

/// Streaming takes the other relay. The model id is swapped on the way out and
/// back, and every other SSE line has to reach Codex byte-for-byte.
#[tokio::test]
async fn responses_passthrough_stream_rewrites_only_the_model() {
    let _guard = TARGET_LOCK.lock().await;
    let seen = Seen::default();

    let provider = serve(
        Router::new()
            .route("/v1/responses", post(upstream_only_responses_stream))
            .with_state(seen.clone()),
    )
    .await;

    set_target(BridgeTarget {
        base_url: format!("http://{provider}/v1"),
        api_key: "test-key".to_string(),
        model: "space-bunny-free".to_string(),
        protocol: WireProtocol::OpenaiResponses,
    });
    let bridge = serve(build_router().unwrap()).await;

    let text = reqwest::Client::new()
        .post(format!("http://{bridge}/v1/responses"))
        .json(&json!({
            "model": "gpt-5.5",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "ping"}]}],
            "stream": true
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let models = seen.models.lock().unwrap().clone();
    println!("provider received models={models:?}");
    println!("client received:\n{text}");

    assert_eq!(
        models,
        vec!["space-bunny-free".to_string()],
        "the provider must receive the real model id, never the gpt-5.5 label"
    );
    assert!(
        !text.contains("space-bunny-free"),
        "the real model id leaked to the client:\n{text}"
    );
    assert!(
        text.contains("\"model\":\"gpt-5.5\""),
        "the client's own model id is missing from the stream:\n{text}"
    );
    // Non-`data:` framing and unknown event types must survive untouched.
    assert!(text.contains("event: response.output_text.delta"), "{text}");
    assert!(text.contains("data: [DONE]"), "{text}");
    assert!(text.contains("\"sequence_number\":7"), "{text}");
    assert!(text.contains("pong"), "{text}");
}
