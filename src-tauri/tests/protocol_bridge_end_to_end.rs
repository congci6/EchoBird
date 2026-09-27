//! The user-facing requirement, proven over real HTTP in both directions.
//!
//! `tool_config_manager`'s unit tests pin down WHICH dialect the bridge is told
//! to target for each client choice. These close the loop the other way: a
//! provider that serves exactly ONE of the four dialects stands in for the
//! supplier behind the requirement, and a client speaking a DIFFERENT dialect
//! reaches it through the bridge.
//!
//! The cases that matter are the mismatched ones, because a matching client and
//! provider prove nothing:
//!
//! - a Gemini client (model in the path) against a Chat Completions provider
//! - a Chat Completions client against an Anthropic Messages provider
//!
//! What each case asserts is the whole contract: the provider received a request
//! in ITS dialect on ITS path, the model id was pinned to the provider's own
//! (the reason this hop exists at all), and the client got an answer back in
//! its OWN dialect — content, tool calls, finish reason and token usage.

use axum::{extract::State, routing::post, Json, Router};
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
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Start the bridge in-process and return its address.
///
/// Unlike the app's own `spawn_proxy_task`, this builds the router directly, so
/// the test never publishes a port to the machine's saved-proxy state and does
/// not depend on a singleton having been started.
async fn serve_bridge() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn post_json(url: &str, body: Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .expect("bridge reachable");
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// A provider serving Chat Completions and nothing else.
///
/// It records the path and model it was called on, and rejects any body that is
/// not a Chat Completions request, so a request left in the client's own dialect
/// fails loudly here instead of passing by accident.
async fn provider_only_chat(
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
    // `messages` is the Chat Completions shape; a body still in Gemini form has
    // `contents` instead and is rejected.
    if body.get("messages").is_none() || body.get("contents").is_some() {
        return Json(
            json!({"error": {"message": "this provider serves /v1/chat/completions only"}}),
        );
    }
    Json(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": "upstream-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "pong",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Beijing\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
    }))
}

/// A provider serving Anthropic Messages and nothing else.
async fn provider_only_messages(
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
    // `max_tokens` is required by Messages and absent from Chat Completions, so
    // its presence is what proves the translation actually ran.
    if body.get("messages").is_none() || body.get("max_tokens").is_none() {
        return Json(json!({"type": "error", "error": {"type": "not_found_error",
                "message": "this provider serves /v1/messages only"}}));
    }
    Json(json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "upstream-model",
        "content": [{"type": "text", "text": "pong"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 11, "output_tokens": 7}
    }))
}

#[tokio::test]
async fn a_gemini_client_reaches_a_chat_only_provider_and_answers_in_gemini() {
    let _guard = TARGET_LOCK.lock().await;
    let seen = Seen::default();
    let provider = serve(
        Router::new()
            .route("/v1/chat/completions", post(provider_only_chat))
            .with_state(seen.clone()),
    )
    .await;

    // Exactly what the central router now builds for a user who picked Gemini
    // against a provider with no Gemini endpoint: the OpenAI base, Chat
    // Completions as the target dialect. Before the fix the target dialect was
    // the user's pick, so the bridge forwarded the Gemini body here untouched
    // and the provider answered 404.
    set_target(BridgeTarget {
        base_url: format!("http://{provider}/v1"),
        api_key: "test-key".to_string(),
        model: "upstream-model".to_string(),
        protocol: WireProtocol::OpenaiChat,
    });
    let bridge = serve_bridge().await;

    let (status, response) = post_json(
        &format!("http://{bridge}/v1beta/models/chosen-model:generateContent"),
        json!({
            "contents": [{"role": "user", "parts": [{"text": "ping"}]}],
            "systemInstruction": {"parts": [{"text": "be terse"}]},
            "tools": [{"functionDeclarations": [{
                "name": "get_weather",
                "description": "weather for a city",
                "parameters": {"type": "object",
                    "properties": {"city": {"type": "string"}}, "required": ["city"]}
            }]}],
            "generationConfig": {"temperature": 0.2, "maxOutputTokens": 64}
        }),
    )
    .await;

    assert_eq!(
        status, 200,
        "a Gemini client must reach a Chat Completions provider: {response}"
    );

    // The provider was called on its own path, in its own dialect.
    assert_eq!(
        seen.paths.lock().unwrap().as_slice(),
        ["/v1/chat/completions"],
        "the request must be translated before it reaches the provider"
    );
    // And the model id pinned to the provider's own, not the client's — this
    // hop is what makes a tool's remembered model id irrelevant.
    assert_eq!(
        seen.models.lock().unwrap().as_slice(),
        ["upstream-model"],
        "the bridge must pin the provider's real model id"
    );

    // The client gets Gemini's shape back, not the provider's. The model id it
    // sees is the one it asked for: the bridge pins the provider's real id on
    // the way OUT (asserted above via `seen.models`) and echoes the caller's own
    // id on the way back, so a tool's remembered model name keeps matching what
    // it sent instead of learning a supplier-internal name.
    assert_eq!(response["modelVersion"], "chosen-model", "{response}");
    let parts = &response["candidates"][0]["content"]["parts"];
    assert_eq!(parts[0]["text"], "pong", "{response}");
    assert_eq!(
        parts[1]["functionCall"]["name"], "get_weather",
        "{response}"
    );
    assert_eq!(
        parts[1]["functionCall"]["args"]["city"], "Beijing",
        "the tool arguments must survive the round trip: {response}"
    );
    assert_eq!(
        response["candidates"][0]["finishReason"], "STOP",
        "{response}"
    );
    assert_eq!(
        response["usageMetadata"]["promptTokenCount"], 11,
        "{response}"
    );
    assert_eq!(
        response["usageMetadata"]["candidatesTokenCount"], 7,
        "{response}"
    );
}

#[tokio::test]
async fn a_chat_client_reaches_a_messages_only_provider_and_answers_in_chat() {
    let _guard = TARGET_LOCK.lock().await;
    let seen = Seen::default();
    let provider = serve(
        Router::new()
            .route("/v1/messages", post(provider_only_messages))
            .with_state(seen.clone()),
    )
    .await;

    set_target(BridgeTarget {
        base_url: format!("http://{provider}"),
        api_key: "test-key".to_string(),
        model: "upstream-model".to_string(),
        protocol: WireProtocol::AnthropicMessages,
    });
    let bridge = serve_bridge().await;

    let (status, response) = post_json(
        &format!("http://{bridge}/v1/chat/completions"),
        json!({
            "model": "chosen-model",
            "stream": false,
            "messages": [{"role": "user", "content": "ping"}]
        }),
    )
    .await;

    assert_eq!(
        status, 200,
        "a Chat Completions client must reach a Messages provider: {response}"
    );
    assert_eq!(
        seen.paths.lock().unwrap().as_slice(),
        ["/v1/messages"],
        "the request must be translated before it reaches the provider"
    );
    assert_eq!(seen.models.lock().unwrap().as_slice(), ["upstream-model"]);

    assert_eq!(response["object"], "chat.completion", "{response}");
    assert_eq!(response["model"], "chosen-model", "{response}");
    assert_eq!(
        response["choices"][0]["message"]["content"], "pong",
        "{response}"
    );
    assert_eq!(
        response["choices"][0]["finish_reason"], "stop",
        "{response}"
    );
    assert_eq!(response["usage"]["prompt_tokens"], 11, "{response}");
    assert_eq!(response["usage"]["completion_tokens"], 7, "{response}");
}
