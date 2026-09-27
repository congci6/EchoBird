//! End-to-end proof of the 503 fix, over real HTTP.
//!
//! A provider that serves ONLY `/v1/chat/completions` stands in for the
//! supplier behind the reported failure. A Codex-shaped Responses request is
//! sent to the bridge's `/v1/responses`; the provider must receive
//! `/v1/chat/completions` carrying the REAL model id, and the answer must come
//! back in Responses shape.

use axum::{extract::State, routing::post, Json, Router};
use echobird_lib::services::protocol::WireProtocol;
use echobird_lib::services::protocol_bridge::{build_router, set_target, BridgeTarget};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Seen {
    paths: Arc<Mutex<Vec<String>>>,
    models: Arc<Mutex<Vec<String>>>,
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

#[tokio::test]
async fn codex_responses_request_reaches_a_chat_only_provider() {
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

    // Exactly the shape Codex sends.
    let response = reqwest::Client::new()
        .post(format!("http://{bridge}/v1/responses"))
        .json(&json!({
            "model": "space-bunny-free",
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
    let text = body["output"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert_eq!(text, "pong", "answer text did not survive the round trip");
    assert_eq!(body["usage"]["total_tokens"], 9);
}
