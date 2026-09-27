//! The reported failure, reproduced end to end and then shown to be gone.
//!
//! The supplier behind the report does not answer `503` for every request — it
//! answers it for every model it has no channel for, which is exactly why the
//! config looked fine while every Codex turn failed. The fake provider below
//! behaves that way: any model other than the configured real id comes back as
//! `503 ... 无可用渠道（distributor）`, and only the real id is served.
//!
//! Codex is then driven the way it really behaves — a Responses request whose
//! `model` is the label it remembers, not the one in `config.toml` — against
//! a supplier speaking each of the four dialects. Every one of them has to
//! come back `200`.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use echobird_lib::services::protocol::WireProtocol;
use echobird_lib::services::protocol_bridge::{build_router, set_target, BridgeTarget};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// The real id EchoBird holds for the supplier. Only this is servable.
const REAL_MODEL: &str = "space-bunny-free";
/// The label Codex remembers and puts on the wire regardless of config.toml.
const LABEL: &str = "gpt-5.5";

#[derive(Clone, Default)]
struct Seen {
    models: Arc<Mutex<Vec<String>>>,
}

/// The supplier's own words, so a failure here is unmistakably the reported
/// one rather than any 500 the harness might invent.
fn no_channel(model: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": {
                "message": format!("分组 user 下模型 {model} 无可用渠道（distributor）"),
                "type": "one_api_error",
                "code": "unknown"
            }
        })),
    )
        .into_response()
}

/// Record what arrived and refuse it unless it is the real id.
fn admit(seen: &Seen, model: &str) -> Option<Response> {
    seen.models.lock().unwrap().push(model.to_string());
    if model == REAL_MODEL {
        None
    } else {
        Some(no_channel(model))
    }
}

fn model_of(body: &Value) -> &str {
    body.get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

async fn chat(State(seen): State<Seen>, Json(body): Json<Value>) -> Response {
    if let Some(refused) = admit(&seen, model_of(&body)) {
        return refused;
    }
    Json(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": REAL_MODEL,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "pong"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}
    }))
    .into_response()
}

async fn responses(State(seen): State<Seen>, Json(body): Json<Value>) -> Response {
    if let Some(refused) = admit(&seen, model_of(&body)) {
        return refused;
    }
    Json(json!({
        "id": "resp_1",
        "object": "response",
        "created_at": 0,
        "model": REAL_MODEL,
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
    .into_response()
}

async fn messages(State(seen): State<Seen>, Json(body): Json<Value>) -> Response {
    if let Some(refused) = admit(&seen, model_of(&body)) {
        return refused;
    }
    Json(json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": REAL_MODEL,
        "content": [{"type": "text", "text": "pong"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 7, "output_tokens": 2}
    }))
    .into_response()
}

/// Gemini addresses the model in the path, so the id arrives as
/// `{model}:generateContent`.
async fn gemini(
    State(seen): State<Seen>,
    Path(model_action): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let model = model_action
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string();
    let _ = body;
    if let Some(refused) = admit(&seen, &model) {
        return refused;
    }
    Json(json!({
        "responseId": "gemini-1",
        "modelVersion": REAL_MODEL,
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "pong"}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 7,
            "candidatesTokenCount": 2,
            "totalTokenCount": 9
        }
    }))
    .into_response()
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A supplier that only ever has the real id, exposed through all four
/// dialects at once.
async fn supplier(seen: Seen) -> SocketAddr {
    serve(
        Router::new()
            .route("/v1/chat/completions", post(chat))
            .route("/v1/responses", post(responses))
            .route("/v1/messages", post(messages))
            .route("/v1beta/models/{model_action}", post(gemini))
            .with_state(seen),
    )
    .await
}

/// The Codex request shape, with the label rather than the real id — this is
/// what produced the report.
fn codex_request() -> Value {
    json!({
        "model": LABEL,
        "input": [{"role": "user", "content": [{"type": "input_text", "text": "ping"}]}],
        "stream": false
    })
}

fn answer_of(body: &Value) -> String {
    body["output"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Prove the fake supplier is faithful before trusting anything it says: the
/// label on the wire is a 503 with the reported text, the real id is a 200.
#[tokio::test]
async fn the_supplier_really_does_refuse_the_label() {
    let seen = Seen::default();
    let addr = supplier(seen.clone()).await;
    let client = reqwest::Client::new();

    let refused = client
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&json!({"model": LABEL, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 503);
    let message = refused.json::<Value>().await.unwrap()["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        message.contains(&format!("模型 {LABEL} 无可用渠道")),
        "the refusal must read like the reported one, got: {message}"
    );

    let served = client
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&json!({"model": REAL_MODEL, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(served.status(), 200, "the real id must be servable");
}

/// The end state: a supplier speaking any of the four dialects answers a
/// Codex turn that carries the `gpt-5.5` label. Before the fix this was a 503
/// on every one of them.
#[tokio::test]
async fn codex_reaches_a_supplier_speaking_any_of_the_four_dialects() {
    for dialect in [
        WireProtocol::OpenaiChat,
        WireProtocol::OpenaiResponses,
        WireProtocol::AnthropicMessages,
        WireProtocol::GeminiGenerateContent,
    ] {
        let seen = Seen::default();
        let addr = supplier(seen.clone()).await;
        let base = match dialect {
            WireProtocol::AnthropicMessages => format!("http://{addr}"),
            WireProtocol::GeminiGenerateContent => format!("http://{addr}/v1beta"),
            _ => format!("http://{addr}/v1"),
        };
        set_target(BridgeTarget {
            base_url: base,
            api_key: "test-key".to_string(),
            model: REAL_MODEL.to_string(),
            protocol: dialect,
        });
        let bridge = serve(build_router().unwrap()).await;

        let response = reqwest::Client::new()
            .post(format!("http://{bridge}/v1/responses"))
            .json(&codex_request())
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();

        let models = seen.models.lock().unwrap().clone();
        println!("{dialect:?} -> {status}; supplier saw {models:?}");

        assert_eq!(
            models,
            vec![REAL_MODEL.to_string()],
            "{dialect:?}: the label reached the supplier"
        );
        assert_eq!(
            status, 200,
            "{dialect:?}: Codex got {status} instead of an answer: {text}"
        );
        let body: Value = serde_json::from_str(&text).expect("a Responses body");
        assert_eq!(answer_of(&body), "pong", "{dialect:?}: wrong answer");
        assert_eq!(
            body["model"], LABEL,
            "{dialect:?}: Codex must still see its own model"
        );
    }
}
