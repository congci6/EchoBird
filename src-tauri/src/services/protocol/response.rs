//! Non-streaming response conversion, in both directions.
//!
//! [`parse_response`] normalizes whatever the upstream returned;
//! [`encode_response`] renders the canonical result in the caller's protocol.

use super::{
    stop_reason_from, stop_reason_to, CanonicalResponse, ContentBlock, Usage, WireProtocol,
};
use serde_json::{json, Map, Value};

/// Normalize a non-streaming upstream response body.
pub fn parse_response(protocol: WireProtocol, body: &Value) -> CanonicalResponse {
    match protocol {
        WireProtocol::OpenaiChat => parse_openai_chat(body),
        WireProtocol::OpenaiResponses => parse_openai_responses(body),
        WireProtocol::AnthropicMessages => parse_anthropic(body),
        WireProtocol::GeminiGenerateContent => parse_gemini(body),
    }
}

fn usage(prompt: u64, completion: u64) -> Usage {
    Usage {
        input_tokens: prompt,
        output_tokens: completion,
    }
}

fn parse_openai_chat(body: &Value) -> CanonicalResponse {
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    let message = choice.and_then(|choice| choice.get("message"));

    let mut content = Vec::new();
    if let Some(text) = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
    {
        if !text.is_empty() {
            content.push(ContentBlock::text(text));
        }
    }
    if let Some(calls) = message
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for (index, call) in calls.iter().enumerate() {
            let function = call.get("function");
            let name = function
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = function
                .and_then(|function| function.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");
            content.push(ContentBlock::ToolUse {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{index}")),
                name: name.to_string(),
                input: serde_json::from_str(arguments).unwrap_or_else(|_| json!({})),
            });
        }
    }

    let raw_usage = body.get("usage");
    CanonicalResponse {
        id: body
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("chatcmpl")
            .to_string(),
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        content,
        stop_reason: choice
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str)
            .map_or(super::StopReason::EndTurn, stop_reason_from),
        usage: usage(
            raw_usage
                .and_then(|usage| usage.get("prompt_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            raw_usage
                .and_then(|usage| usage.get("completion_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ),
    }
}

fn parse_openai_responses(body: &Value) -> CanonicalResponse {
    let mut content = Vec::new();
    for item in body
        .get("output")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                let text = item.get("summary").map(flatten).unwrap_or_default();
                if !text.is_empty() {
                    content.push(ContentBlock::Thinking {
                        text,
                        signature: None,
                    });
                }
            }
            Some("function_call") => {
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                content.push(ContentBlock::ToolUse {
                    id: item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: serde_json::from_str(arguments).unwrap_or_else(|_| json!({})),
                });
            }
            _ => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                {
                    match part.get("type").and_then(Value::as_str) {
                        Some("output_text") | Some("text") => {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    content.push(ContentBlock::text(text));
                                }
                            }
                        }
                        Some("refusal") => {
                            if let Some(text) = part.get("refusal").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    content.push(ContentBlock::text(text));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    let raw_usage = body.get("usage");
    // A unction_call item means the turn ended on a tool call. Decided
    // before content is moved into the struct below.
    let has_tool_call = content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
    CanonicalResponse {
        id: body
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("resp")
            .to_string(),
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        content,
        // A `function_call` item means the turn ended on a tool call; decided
        // before `content` was moved into this struct.
        stop_reason: if has_tool_call {
            super::StopReason::ToolUse
        } else {
            match body.get("status").and_then(Value::as_str) {
                Some("incomplete") => super::StopReason::MaxTokens,
                _ => super::StopReason::EndTurn,
            }
        },
        usage: usage(
            raw_usage
                .and_then(|usage| usage.get("input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            raw_usage
                .and_then(|usage| usage.get("output_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ),
    }
}

fn parse_anthropic(body: &Value) -> CanonicalResponse {
    let mut content = Vec::new();
    for block in body
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        content.push(ContentBlock::text(text));
                    }
                }
            }
            Some("thinking") => {
                content.push(ContentBlock::Thinking {
                    text: block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    signature: block
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
            Some("tool_use") => {
                content.push(ContentBlock::ToolUse {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: block.get("input").cloned().unwrap_or_else(|| json!({})),
                });
            }
            _ => {}
        }
    }

    let raw_usage = body.get("usage");
    CanonicalResponse {
        id: body
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("msg")
            .to_string(),
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        content,
        stop_reason: body
            .get("stop_reason")
            .and_then(Value::as_str)
            .map_or(super::StopReason::EndTurn, stop_reason_from),
        usage: usage(
            raw_usage
                .and_then(|usage| usage.get("input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            raw_usage
                .and_then(|usage| usage.get("output_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ),
    }
}

fn parse_gemini(body: &Value) -> CanonicalResponse {
    let candidate = body
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first());

    let mut content = Vec::new();
    for part in candidate
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if text.is_empty() {
                continue;
            }
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                content.push(ContentBlock::Thinking {
                    text: text.to_string(),
                    signature: None,
                });
            } else {
                content.push(ContentBlock::text(text));
            }
        }
        if let Some(call) = part.get("functionCall") {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            content.push(ContentBlock::ToolUse {
                // Gemini has no call id; mint a stable one from the position so
                // a following functionResponse can be matched by name.
                id: format!("call_{}", content.len()),
                name,
                input: call.get("args").cloned().unwrap_or_else(|| json!({})),
            });
        }
    }

    let raw_usage = body.get("usageMetadata");
    CanonicalResponse {
        id: body
            .get("responseId")
            .and_then(Value::as_str)
            .unwrap_or("gemini")
            .to_string(),
        model: body
            .get("modelVersion")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        content,
        stop_reason: candidate
            .and_then(|candidate| candidate.get("finishReason"))
            .and_then(Value::as_str)
            .map_or(super::StopReason::EndTurn, stop_reason_from),
        usage: usage(
            raw_usage
                .and_then(|usage| usage.get("promptTokenCount"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            raw_usage
                .and_then(|usage| usage.get("candidatesTokenCount"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ),
    }
}

// ─── Encoding back to the caller ───

/// Render a canonical response in `protocol` for the calling client.
/// `stream` is false here; streaming output goes through `stream`.
pub fn encode_response(
    protocol: WireProtocol,
    response: &CanonicalResponse,
    requested_model: &str,
) -> Value {
    match protocol {
        WireProtocol::OpenaiChat => encode_openai_chat(response, requested_model),
        WireProtocol::OpenaiResponses => encode_openai_responses(response, requested_model),
        WireProtocol::AnthropicMessages => encode_anthropic(response, requested_model),
        WireProtocol::GeminiGenerateContent => encode_gemini(response, requested_model),
    }
}

fn encode_openai_chat(response: &CanonicalResponse, model: &str) -> Value {
    let text = response.joined_text();
    let tool_calls: Vec<Value> = response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string()),
                }
            })),
            _ => None,
        })
        .collect();

    let mut message = Map::new();
    message.insert("role".to_string(), json!("assistant"));
    message.insert("content".to_string(), json!(text));
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    json!({
        "id": response.id,
        "object": "chat.completion",
        "created": crate::services::protocol::now_seconds(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": stop_reason_to(WireProtocol::OpenaiChat, response.stop_reason),
        }],
        "usage": {
            "prompt_tokens": response.usage.input_tokens,
            "completion_tokens": response.usage.output_tokens,
            "total_tokens": response.usage.input_tokens + response.usage.output_tokens,
        }
    })
}

fn encode_openai_responses(response: &CanonicalResponse, model: &str) -> Value {
    let text = response.joined_text();
    let mut output = Vec::new();
    if !text.is_empty() {
        output.push(json!({
            "type": "message",
            "id": format!("msg_{}", response.id),
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
    }
    for block in &response.content {
        if let ContentBlock::ToolUse { id, name, input } = block {
            output.push(json!({
                "type": "function_call",
                "id": format!("fc_{id}"),
                "call_id": id,
                "name": name,
                "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string()),
                "status": "completed",
            }));
        }
    }

    json!({
        "id": response.id,
        "object": "response",
        "created_at": crate::services::protocol::now_seconds(),
        "model": model,
        "status": if response.stop_reason == super::StopReason::MaxTokens {
            "incomplete"
        } else {
            "completed"
        },
        "output": output,
        "usage": {
            "input_tokens": response.usage.input_tokens,
            "output_tokens": response.usage.output_tokens,
            "total_tokens": response.usage.input_tokens + response.usage.output_tokens,
        }
    })
}

fn encode_anthropic(response: &CanonicalResponse, model: &str) -> Value {
    let content: Vec<Value> = response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
            ContentBlock::ToolUse { id, name, input } => {
                Some(json!({"type": "tool_use", "id": id, "name": name, "input": input}))
            }
            _ => None,
        })
        .collect();

    json!({
        "id": response.id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason_to(WireProtocol::AnthropicMessages, response.stop_reason),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": response.usage.input_tokens,
            "output_tokens": response.usage.output_tokens,
        }
    })
}

fn encode_gemini(response: &CanonicalResponse, model: &str) -> Value {
    let mut parts = Vec::new();
    let text = response.joined_text();
    if !text.is_empty() {
        parts.push(json!({"text": text}));
    }
    for block in &response.content {
        if let ContentBlock::ToolUse { name, input, .. } = block {
            parts.push(json!({"functionCall": {"name": name, "args": input}}));
        }
    }

    json!({
        "candidates": [{
            "content": {"role": "model", "parts": parts},
            "finishReason": stop_reason_to(WireProtocol::GeminiGenerateContent, response.stop_reason),
            "index": 0,
        }],
        "usageMetadata": {
            "promptTokenCount": response.usage.input_tokens,
            "candidatesTokenCount": response.usage.output_tokens,
            "totalTokenCount": response.usage.input_tokens + response.usage.output_tokens,
        },
        "modelVersion": model,
        "responseId": response.id,
    })
}

/// Render a structured value as text for protocols that only carry strings.
fn flatten(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(flatten).collect(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_response() -> CanonicalResponse {
        CanonicalResponse {
            id: "resp_1".to_string(),
            model: "upstream-model".to_string(),
            content: vec![
                ContentBlock::text("here you go"),
                ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "lookup".to_string(),
                    input: json!({"q": "x"}),
                },
            ],
            stop_reason: super::super::StopReason::ToolUse,
            usage: Usage {
                input_tokens: 11,
                output_tokens: 7,
            },
        }
    }

    #[test]
    fn chat_upstream_parses_text_tools_and_usage() {
        let body = json!({
            "id": "chatcmpl-1",
            "model": "gpt-x",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "hello",
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {"name": "f", "arguments": "{\"a\":1}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4}
        });
        let parsed = parse_openai_chat(&body);
        assert_eq!(parsed.id, "chatcmpl-1");
        assert_eq!(parsed.joined_text(), "hello");
        assert_eq!(parsed.stop_reason, super::super::StopReason::ToolUse);
        assert_eq!(parsed.usage.input_tokens, 3);
        assert_eq!(parsed.content.len(), 2);
    }

    #[test]
    fn responses_upstream_parses_function_calls() {
        let body = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "hi"}]},
                {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"}
            ],
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let parsed = parse_openai_responses(&body);
        assert_eq!(parsed.joined_text(), "hi");
        assert_eq!(parsed.stop_reason, super::super::StopReason::ToolUse);
    }

    #[test]
    fn anthropic_upstream_parses_thinking_and_tools() {
        let body = json!({
            "id": "msg_1",
            "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                {"type": "text", "text": "answer"},
                {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5, "output_tokens": 6}
        });
        let parsed = parse_anthropic(&body);
        assert_eq!(parsed.content.len(), 3);
        assert_eq!(parsed.joined_text(), "answer");
        assert_eq!(parsed.stop_reason, super::super::StopReason::ToolUse);
    }

    #[test]
    fn gemini_upstream_parses_parts_and_usage() {
        let body = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"text": "hey"},
                    {"functionCall": {"name": "f", "args": {"a": 1}}}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 3}
        });
        let parsed = parse_gemini(&body);
        assert_eq!(parsed.joined_text(), "hey");
        assert_eq!(parsed.stop_reason, super::super::StopReason::EndTurn);
        assert_eq!(parsed.usage.output_tokens, 3);
    }

    #[test]
    fn every_protocol_encodes_a_tool_call_response() {
        let response = tool_response();
        for protocol in [
            WireProtocol::OpenaiChat,
            WireProtocol::OpenaiResponses,
            WireProtocol::AnthropicMessages,
            WireProtocol::GeminiGenerateContent,
        ] {
            let body = encode_response(protocol, &response, "client-model");
            // The rendered body must always mention the tool name and the
            // canonical id, whichever protocol the caller speaks.
            let rendered = serde_json::to_string(&body).unwrap();
            assert!(rendered.contains("lookup"), "{protocol} lost the tool name");
            // Gemini correlates a functionResponse by NAME, not by id, so it is
            // the one protocol that legitimately carries no call id.
            if protocol != WireProtocol::GeminiGenerateContent {
                assert!(rendered.contains("call_1"), "{protocol} lost the call id");
            }
            assert!(
                rendered.contains("client-model"),
                "{protocol} must echo the model the caller asked for"
            );
        }
    }

    #[test]
    fn openai_chat_encoding_uses_the_expected_shape() {
        let body = encode_openai_chat(&tool_response(), "m");
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_1"
        );
        assert_eq!(body["usage"]["total_tokens"], 18);
    }

    #[test]
    fn anthropic_encoding_omits_thinking_blocks() {
        let body = encode_anthropic(&tool_response(), "m");
        let content = body["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert!(content.iter().all(|part| part["type"] != "thinking"));
        assert_eq!(body["stop_reason"], "tool_use");
    }
}
