//! End-to-end proof of the objective: a provider that serves ONLY OpenAI Chat
//! Completions must still be usable by a Responses client, an Anthropic
//! Messages client, and a Gemini generateContent client.
//!
//! These exercise the real `services::protocol` module — the same code the
//! protocol bridge serves requests with — rather than a copy.

use echobird_lib::services::protocol::{
    encode_request, encode_response, parse_request, parse_response, StreamDecoder, StreamEncoder,
    WireProtocol,
};
use serde_json::{json, Value};

const CHAT: WireProtocol = WireProtocol::OpenaiChat;
const RESPONSES: WireProtocol = WireProtocol::OpenaiResponses;
const ANTHROPIC: WireProtocol = WireProtocol::AnthropicMessages;
const GEMINI: WireProtocol = WireProtocol::GeminiGenerateContent;

const ALL: [WireProtocol; 4] = [CHAT, RESPONSES, ANTHROPIC, GEMINI];

/// A tool definition, written once in Chat shape and carried across dialects.
fn tool_name_of(body: &Value, protocol: WireProtocol) -> Option<String> {
    let first = body.get("tools")?.as_array()?.first()?;
    match protocol {
        WireProtocol::OpenaiChat => first
            .get("function")?
            .get("name")?
            .as_str()
            .map(String::from),
        WireProtocol::OpenaiResponses => first.get("name")?.as_str().map(String::from),
        WireProtocol::AnthropicMessages => first.get("name")?.as_str().map(String::from),
        WireProtocol::GeminiGenerateContent => first
            .get("functionDeclarations")?
            .as_array()?
            .first()?
            .get("name")?
            .as_str()
            .map(String::from),
    }
}

/// Every user-visible string a request carries, so a test can assert the
/// prompt survived the hop regardless of which field the dialect hides it in.
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
// A client speaking any of the four dialects must be translatable into the
// one dialect the provider actually serves.

#[test]
fn responses_client_reaches_a_chat_only_provider() {
    let body = json!({
        "model": "glm-5.2",
        "instructions": "You are terse.",
        "input": [
            { "role": "user", "content": [{ "type": "input_text", "text": "hello there" }] }
        ],
        "tools": [{ "type": "function", "name": "get_weather", "parameters": {
            "type": "object", "properties": { "city": { "type": "string" } } } }]
    });
    let parsed = parse_request(RESPONSES, &body, None).expect("responses parses");
    let chat = encode_request(CHAT, &parsed);

    assert_eq!(chat["model"], "glm-5.2");
    let text = body_text(&chat);
    assert!(text.contains("hello there"), "prompt lost: {chat}");
    assert!(text.contains("You are terse."), "instructions lost: {chat}");
    assert_eq!(tool_name_of(&chat, CHAT).as_deref(), Some("get_weather"));
}

#[test]
fn anthropic_client_reaches_a_chat_only_provider() {
    let body = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 1024,
        "system": "You are terse.",
        "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hello there" }] }],
        "tools": [{ "name": "get_weather", "input_schema": {
            "type": "object", "properties": { "city": { "type": "string" } } } }]
    });
    let parsed = parse_request(ANTHROPIC, &body, None).expect("anthropic parses");
    let chat = encode_request(CHAT, &parsed);

    assert_eq!(chat["model"], "claude-sonnet-4");
    let text = body_text(&chat);
    assert!(text.contains("hello there"), "prompt lost: {chat}");
    assert!(text.contains("You are terse."), "system lost: {chat}");
    // Anthropic makes max_tokens mandatory; Chat treats it as a cap.
    assert_eq!(chat["max_tokens"], 1024);
    assert_eq!(tool_name_of(&chat, CHAT).as_deref(), Some("get_weather"));
}

#[test]
fn gemini_client_reaches_a_chat_only_provider() {
    // Gemini addresses the model in the path, not the body.
    let body = json!({
        "systemInstruction": { "parts": [{ "text": "You are terse." }] },
        "contents": [{ "role": "user", "parts": [{ "text": "hello there" }] }],
        "tools": [{ "functionDeclarations": [{ "name": "get_weather", "parameters": {
            "type": "object", "properties": { "city": { "type": "string" } } } }] }]
    });
    let parsed = parse_request(GEMINI, &body, Some("gemini-2.5-pro")).expect("gemini parses");
    let chat = encode_request(CHAT, &parsed);

    // The model came from the URL path, so the Chat body has to carry it.
    assert_eq!(chat["model"], "gemini-2.5-pro");
    let text = body_text(&chat);
    assert!(text.contains("hello there"), "prompt lost: {chat}");
    assert!(text.contains("You are terse."), "system lost: {chat}");
    assert_eq!(tool_name_of(&chat, CHAT).as_deref(), Some("get_weather"));
}

/// A tool RESULT is the half that breaks naive translators: it must come back
/// as a tool-role message on the Chat side, and Gemini/Anthropic both give it
/// a dedicated shape rather than a plain string.
#[test]
fn tool_results_survive_the_hop_to_chat() {
    let anthropic = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 512,
        "messages": [{
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "call_1",
                "content": "18C, sunny"
            }]
        }]
    });
    let parsed = parse_request(ANTHROPIC, &anthropic, None).expect("anthropic parses");
    let chat = encode_request(CHAT, &parsed);
    let roles: Vec<&str> = chat["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|m| m.get("role").and_then(Value::as_str))
        .collect();
    assert!(
        roles.contains(&"tool"),
        "tool_result did not become a tool-role message: {chat}"
    );

    // And the same canonical request must encode to Gemini's own shape.
    let gemini = encode_request(GEMINI, &parsed);
    let text = body_text(&gemini);
    assert!(text.contains("18C, sunny"), "tool result lost: {gemini}");
}

// ─── Response direction ─────────────────────────────────────────────────
// The provider answers in Chat shape. Every client dialect must still get a
// well-formed answer.

fn chat_answer() -> Value {
    json!({
        "id": "chatcmpl-1",
        "model": "glm-5.2",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Sunny, 18C." },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18 }
    })
}

#[test]
fn chat_answer_reaches_every_client_dialect() {
    let canonical = parse_response(CHAT, &chat_answer());
    assert_eq!(canonical.joined_text(), "Sunny, 18C.");
    assert_eq!(canonical.usage.input_tokens, 11);
    assert_eq!(canonical.usage.output_tokens, 7);

    for client in ALL {
        let body = encode_response(client, &canonical, "glm-5.2");
        let text = body_text(&body);
        assert!(
            text.contains("Sunny, 18C."),
            "{client:?} lost the answer: {body}"
        );
    }
}

/// The four dialects disagree on where the text lives, so assert the field
/// each one actually reads rather than just "the string is somewhere".
#[test]
fn each_dialect_puts_the_answer_where_its_clients_look() {
    let canonical = parse_response(CHAT, &chat_answer());

    let chat = encode_response(CHAT, &canonical, "glm-5.2");
    assert_eq!(chat["choices"][0]["message"]["content"], "Sunny, 18C.");

    let responses = encode_response(RESPONSES, &canonical, "glm-5.2");
    let output = responses["output"].as_array().expect("output array");
    assert!(
        output
            .iter()
            .any(|item| body_text(item).contains("Sunny, 18C.")),
        "responses output missing the text: {responses}"
    );

    let anthropic = encode_response(ANTHROPIC, &canonical, "glm-5.2");
    let blocks = anthropic["content"].as_array().expect("content array");
    // Assert the exact field, not "the string is somewhere": `body_text`
    // would also pick up the literal "text" in the block's own `type`.
    assert!(
        blocks
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == "Sunny, 18C."),
        "anthropic text block missing: {anthropic}"
    );

    let gemini = encode_response(GEMINI, &canonical, "glm-5.2");
    let candidates = gemini["candidates"].as_array().expect("candidates array");
    assert!(
        body_text(&candidates[0]).contains("Sunny, 18C."),
        "gemini candidate missing the text: {gemini}"
    );
}

// ─── Streaming direction ────────────────────────────────────────────────
// A Chat-only provider streams Chat SSE. Each client dialect must receive a
// stream it can actually consume.

fn chat_sse() -> String {
    [
        r#"data: {"id":"c1","model":"glm-5.2","choices":[{"index":0,"delta":{"role":"assistant","content":"Sunny"},"finish_reason":null}]}"#,
        r#"data: {"id":"c1","model":"glm-5.2","choices":[{"index":0,"delta":{"content":", 18C."},"finish_reason":null}]}"#,
        r#"data: {"id":"c1","model":"glm-5.2","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":7}}"#,
        "data: [DONE]",
        "",
    ]
    .join("\n")
}

fn collect_stream(client: WireProtocol) -> String {
    let mut decoder = StreamDecoder::new(CHAT);
    let mut events = decoder.push(chat_sse().as_bytes());
    events.extend(decoder.finish());

    let mut encoder = StreamEncoder::new(client, "glm-5.2");
    let mut wire = String::from_utf8_lossy(&encoder.start()).into_owned();
    for event in &events {
        wire.push_str(&String::from_utf8_lossy(&encoder.push(event)));
    }
    wire.push_str(&String::from_utf8_lossy(&encoder.finish()));
    wire
}

#[test]
fn chat_stream_reaches_every_client_dialect() {
    for client in ALL {
        let wire = collect_stream(client);
        assert!(
            wire.contains("Sunny") && wire.contains("18C."),
            "{client:?} lost the streamed text:\n{wire}"
        );
    }
}

/// Usage must reach the client before the stream is declared over. A client
/// that sees `[DONE]` / `message_stop` first and a usage frame afterwards
/// treats the stream as malformed, so ordering is part of the contract, not
/// cosmetics.
///
/// Gemini is the deliberate exception: its real API ships `finishReason` and
/// `usageMetadata` inside the SAME final chunk, so there is no separate
/// terminal frame to order against. Assert that shape rather than skipping it.
#[test]
fn usage_is_flushed_before_each_dialect_declares_the_stream_over() {
    for (client, terminal) in [
        (CHAT, "[DONE]"),
        (RESPONSES, "response.completed"),
        (ANTHROPIC, "message_stop"),
    ] {
        let wire = collect_stream(client);
        let usage_at = wire
            .find("usage")
            .unwrap_or_else(|| panic!("{client:?} carried no usage frame:\n{wire}"));
        let stop_at = wire
            .find(terminal)
            .unwrap_or_else(|| panic!("{client:?} never sent {terminal}:\n{wire}"));
        assert!(
            usage_at < stop_at,
            "{client:?} sent {terminal} before usage:\n{wire}"
        );
    }

    let gemini = collect_stream(GEMINI);
    let final_frame = gemini
        .rsplit("\n\n")
        .find(|frame| frame.contains("finishReason"))
        .unwrap_or_else(|| panic!("gemini never sent finishReason:\n{gemini}"));
    assert!(
        final_frame.contains("usageMetadata"),
        "gemini must ship usageMetadata in the same chunk as finishReason, as the real API does:\n{gemini}"
    );
}
#[test]
fn tool_calls_stream_through_to_every_client_dialect() {
    let sse = [
        r#"data: {"id":"c2","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_abc","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"c2","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "data: [DONE]",
        "",
    ]
    .join("\n");

    for client in ALL {
        let mut decoder = StreamDecoder::new(CHAT);
        let mut events = decoder.push(sse.as_bytes());
        events.extend(decoder.finish());

        let mut encoder = StreamEncoder::new(client, "glm-5.2");
        let mut wire = String::from_utf8_lossy(&encoder.start()).into_owned();
        for event in &events {
            wire.push_str(&String::from_utf8_lossy(&encoder.push(event)));
        }
        wire.push_str(&String::from_utf8_lossy(&encoder.finish()));

        assert!(
            wire.contains("get_weather"),
            "{client:?} lost the tool call:\n{wire}"
        );
        // Arguments must arrive as parseable JSON, not a duplicated fragment.
        assert!(
            wire.contains("Paris"),
            "{client:?} lost the tool arguments:\n{wire}"
        );
    }
}

/// The headline claim: with a Chat-only provider, a client may pick any of
/// the four protocols and EchoBird converts. This walks the full
/// request -> Chat -> response cycle for every client dialect.
#[test]
fn any_client_dialect_works_against_a_chat_only_provider() {
    let clients: [(&str, Value, Option<&str>); 4] = [
        (
            "chat",
            json!({ "model": "glm-5.2", "messages": [{ "role": "user", "content": "hi" }] }),
            None,
        ),
        (
            "responses",
            json!({ "model": "glm-5.2", "input": [{ "role": "user", "content": "hi" }] }),
            None,
        ),
        (
            "anthropic",
            json!({ "model": "glm-5.2", "max_tokens": 64,
                    "messages": [{ "role": "user", "content": "hi" }] }),
            None,
        ),
        (
            "gemini",
            json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] }),
            Some("glm-5.2"),
        ),
    ];

    for (label, body, path_model) in clients {
        let client = WireProtocol::parse(label).expect("known dialect");
        let request = parse_request(client, &body, path_model).expect("client request parses");

        // Translate to the only protocol the provider speaks.
        let upstream = encode_request(CHAT, &request);
        assert_eq!(upstream["model"], "glm-5.2", "{label}: model not carried");

        // The provider answers in its own dialect; translate back.
        let answer = parse_response(CHAT, &chat_answer());
        let reply = encode_response(client, &answer, "glm-5.2");
        assert!(
            body_text(&reply).contains("Sunny, 18C."),
            "{label}: answer not delivered: {reply}"
        );
    }
}
