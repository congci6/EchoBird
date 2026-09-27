//! Mirror of the Chat-only case in `protocol_conversion.rs`: a provider that
//! serves ONLY OpenAI Responses must still be usable by Chat Completions,
//! Anthropic Messages and Gemini generateContent clients.
//!
//! The conversion layer is meant to be symmetric, so this file exists to keep
//! it that way — a fix to the Chat-only direction must not quietly break the
//! Responses-only one.

use echobird_lib::services::protocol::{
    encode_request, encode_response, parse_request, parse_response, StreamDecoder, StreamEncoder,
    WireProtocol,
};
use serde_json::{json, Value};

const CHAT: WireProtocol = WireProtocol::OpenaiChat;
const RESPONSES: WireProtocol = WireProtocol::OpenaiResponses;
const ANTHROPIC: WireProtocol = WireProtocol::AnthropicMessages;
const GEMINI: WireProtocol = WireProtocol::GeminiGenerateContent;

fn body_text(body: &Value) -> String {
    let mut out = String::new();
    fn walk(v: &Value, out: &mut String) {
        match v {
            Value::String(s) => out.push_str(s),
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    walk(body, &mut out);
    out
}

// ─── Request direction ──────────────────────────────────────────────────

#[test]
fn chat_client_reaches_a_responses_only_provider() {
    let body = json!({
        "model": "gpt-5.5",
        "messages": [
            { "role": "system", "content": "You are terse." },
            { "role": "user", "content": "hello there" }
        ],
        "tools": [{ "type": "function", "function": { "name": "get_weather",
            "parameters": { "type": "object", "properties": { "city": { "type": "string" } } } } }]
    });
    let parsed = parse_request(CHAT, &body, None).expect("chat parses");
    let responses = encode_request(RESPONSES, &parsed);

    assert_eq!(responses["model"], "gpt-5.5");
    // Responses takes system text out of band, in `instructions`.
    assert_eq!(responses["instructions"], "You are terse.");
    let text = body_text(&responses);
    assert!(text.contains("hello there"), "prompt lost: {responses}");
    // Responses tools are flat, not nested under a `function` key.
    let tools = responses["tools"].as_array().expect("tools array");
    assert!(
        tools.iter().any(|t| t["name"] == "get_weather"),
        "tool not flattened for Responses: {responses}"
    );
}

#[test]
fn anthropic_client_reaches_a_responses_only_provider() {
    let body = json!({
        "model": "gpt-5.5",
        "max_tokens": 1024,
        "system": "You are terse.",
        "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hello there" }] }],
        "tools": [{ "name": "get_weather", "input_schema": {
            "type": "object", "properties": { "city": { "type": "string" } } } }]
    });
    let parsed = parse_request(ANTHROPIC, &body, None).expect("anthropic parses");
    let responses = encode_request(RESPONSES, &parsed);

    assert_eq!(responses["model"], "gpt-5.5");
    assert_eq!(responses["instructions"], "You are terse.");
    let text = body_text(&responses);
    assert!(text.contains("hello there"), "prompt lost: {responses}");
    let tools = responses["tools"].as_array().expect("tools array");
    assert!(
        tools.iter().any(|t| t["name"] == "get_weather"),
        "tool not flattened for Responses: {responses}"
    );
}

#[test]
fn gemini_client_reaches_a_responses_only_provider() {
    let body = json!({
        "systemInstruction": { "parts": [{ "text": "You are terse." }] },
        "contents": [{ "role": "user", "parts": [{ "text": "hello there" }] }],
        "tools": [{ "functionDeclarations": [{ "name": "get_weather", "parameters": {
            "type": "object", "properties": { "city": { "type": "string" } } } }] }]
    });
    let parsed = parse_request(GEMINI, &body, Some("gpt-5.5")).expect("gemini parses");
    let responses = encode_request(RESPONSES, &parsed);

    // Gemini addresses the model in the path; Responses carries it in the body.
    assert_eq!(responses["model"], "gpt-5.5");
    assert_eq!(responses["instructions"], "You are terse.");
    let text = body_text(&responses);
    assert!(text.contains("hello there"), "prompt lost: {responses}");
    let tools = responses["tools"].as_array().expect("tools array");
    assert!(
        tools.iter().any(|t| t["name"] == "get_weather"),
        "tool not flattened for Responses: {responses}"
    );
}

#[test]
fn tool_results_survive_the_hop_to_responses() {
    let anthropic = json!({
        "model": "gpt-5.5",
        "max_tokens": 512,
        "messages": [{
            "role": "user",
            "content": [{ "type": "tool_result", "tool_use_id": "call_1", "content": "18C, sunny" }]
        }]
    });
    let parsed = parse_request(ANTHROPIC, &anthropic, None).expect("anthropic parses");
    let responses = encode_request(RESPONSES, &parsed);
    let text = body_text(&responses);
    assert!(text.contains("18C, sunny"), "tool result lost: {responses}");
    // Responses models a tool result as a function_call_output item.
    assert!(
        body_text(&responses).contains("function_call_output")
            || responses["input"]
                .as_array()
                .is_some_and(|items| items.iter().any(|i| i["type"] == "function_call_output")),
        "tool result did not become a function_call_output item: {responses}"
    );
}

// ─── Response direction ─────────────────────────────────────────────────

fn responses_answer() -> Value {
    json!({
        "id": "resp_1",
        "object": "response",
        "model": "gpt-5.5",
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "text": "Sunny, 18C." }]
        }],
        "usage": { "input_tokens": 11, "output_tokens": 7, "total_tokens": 18 }
    })
}

#[test]
fn responses_answer_reaches_every_other_client_dialect() {
    let canonical = parse_response(RESPONSES, &responses_answer());
    assert_eq!(canonical.joined_text(), "Sunny, 18C.");
    assert_eq!(canonical.usage.input_tokens, 11);
    assert_eq!(canonical.usage.output_tokens, 7);

    for client in [CHAT, RESPONSES, ANTHROPIC, GEMINI] {
        let body = encode_response(client, &canonical, "gpt-5.5");
        assert!(
            body_text(&body).contains("Sunny, 18C."),
            "{client:?} lost the answer: {body}"
        );
    }
}

#[test]
fn each_dialect_puts_a_responses_answer_where_its_clients_look() {
    let canonical = parse_response(RESPONSES, &responses_answer());

    let chat = encode_response(CHAT, &canonical, "gpt-5.5");
    assert_eq!(chat["choices"][0]["message"]["content"], "Sunny, 18C.");

    let anthropic = encode_response(ANTHROPIC, &canonical, "gpt-5.5");
    let blocks = anthropic["content"].as_array().expect("content array");
    assert!(
        blocks
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == "Sunny, 18C."),
        "anthropic text block missing: {anthropic}"
    );

    let gemini = encode_response(GEMINI, &canonical, "gpt-5.5");
    let candidates = gemini["candidates"].as_array().expect("candidates array");
    assert!(
        body_text(&candidates[0]).contains("Sunny, 18C."),
        "gemini candidate missing the text: {gemini}"
    );
}

// ─── Streaming direction ────────────────────────────────────────────────

fn responses_sse() -> String {
    [
        r#"event: response.created"#,
        r#"data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5","status":"in_progress"}}"#,
        "",
        r#"event: response.output_text.delta"#,
        r#"data: {"type":"response.output_text.delta","delta":"Sunny"}"#,
        "",
        r#"event: response.output_text.delta"#,
        r#"data: {"type":"response.output_text.delta","delta":", 18C."}"#,
        "",
        r#"event: response.completed"#,
        r#"data: {"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.5","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Sunny, 18C."}]}],"usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18}}}"#,
        "",
        "",
    ]
    .join("\n")
}

#[test]
fn responses_stream_reaches_every_other_client_dialect() {
    for client in [CHAT, ANTHROPIC, GEMINI] {
        let mut decoder = StreamDecoder::new(RESPONSES);
        let mut events = decoder.push(responses_sse().as_bytes());
        events.extend(decoder.finish());

        let mut encoder = StreamEncoder::new(client, "gpt-5.5");
        let mut wire = String::from_utf8_lossy(&encoder.start()).into_owned();
        for event in &events {
            wire.push_str(&String::from_utf8_lossy(&encoder.push(event)));
        }
        wire.push_str(&String::from_utf8_lossy(&encoder.finish()));

        assert!(
            wire.contains("Sunny") && wire.contains("18C."),
            "{client:?} lost the streamed text:\n{wire}"
        );
    }
}

#[test]
fn usage_is_flushed_before_a_responses_stream_declares_itself_over() {
    for (client, terminal) in [
        (CHAT, "[DONE]"),
        (ANTHROPIC, "message_stop"),
        (GEMINI, "finishReason"),
    ] {
        let mut decoder = StreamDecoder::new(RESPONSES);
        let mut events = decoder.push(responses_sse().as_bytes());
        events.extend(decoder.finish());

        let mut encoder = StreamEncoder::new(client, "gpt-5.5");
        let mut wire = String::from_utf8_lossy(&encoder.start()).into_owned();
        for event in &events {
            wire.push_str(&String::from_utf8_lossy(&encoder.push(event)));
        }
        wire.push_str(&String::from_utf8_lossy(&encoder.finish()));

        let usage_at = wire
            .find("usage")
            .unwrap_or_else(|| panic!("{client:?} carried no usage frame:\n{wire}"));
        let stop_at = wire
            .find(terminal)
            .unwrap_or_else(|| panic!("{client:?} never sent {terminal}:\n{wire}"));
        // Gemini ships finishReason and usageMetadata in one frame, so only
        // the dialects with a separate terminal frame can be ordered.
        if client != GEMINI {
            assert!(
                usage_at < stop_at,
                "{client:?} sent {terminal} before usage:\n{wire}"
            );
        }
    }
}

// ─── The headline mirror claim ──────────────────────────────────────────

#[test]
fn any_client_dialect_works_against_a_responses_only_provider() {
    let clients: [(&str, Value, Option<&str>); 4] = [
        (
            "chat",
            json!({ "model": "gpt-5.5", "messages": [{ "role": "user", "content": "hi" }] }),
            None,
        ),
        (
            "responses",
            json!({ "model": "gpt-5.5", "input": [{ "role": "user", "content": "hi" }] }),
            None,
        ),
        (
            "anthropic",
            json!({ "model": "gpt-5.5", "max_tokens": 64,
                    "messages": [{ "role": "user", "content": "hi" }] }),
            None,
        ),
        (
            "gemini",
            json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] }),
            Some("gpt-5.5"),
        ),
    ];

    for (label, body, path_model) in clients {
        let client = WireProtocol::parse(label).expect("known dialect");
        let request = parse_request(client, &body, path_model).expect("client request parses");

        // Translate to the only protocol the provider speaks.
        let upstream = encode_request(RESPONSES, &request);
        assert_eq!(upstream["model"], "gpt-5.5", "{label}: model not carried");

        // The provider answers in its own dialect; translate back.
        let answer = parse_response(RESPONSES, &responses_answer());
        let reply = encode_response(client, &answer, "gpt-5.5");
        assert!(
            body_text(&reply).contains("Sunny, 18C."),
            "{label}: answer not delivered: {reply}"
        );
    }
}
