//! Outbound request encoding: [`CanonicalRequest`] -> each wire protocol.
//!
//! This is the mirror of [`crate::services::protocol::request`]. Together the
//! two modules let any inbound protocol reach any upstream protocol, which is
//! what makes a provider that only speaks one dialect usable through all four.

use super::{CanonicalRequest, ContentBlock, Role, StopReason, WireProtocol};
use serde_json::{json, Map, Value};

/// Anthropic rejects requests without `max_tokens`; use this when the caller
/// did not specify one.
const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 8_192;

/// Encode a canonical request as `protocol`.
pub fn encode_request(protocol: WireProtocol, request: &CanonicalRequest) -> Value {
    match protocol {
        WireProtocol::OpenaiChat => encode_openai_chat(request),
        WireProtocol::OpenaiResponses => encode_openai_responses(request),
        WireProtocol::AnthropicMessages => encode_anthropic(request),
        WireProtocol::GeminiGenerateContent => encode_gemini(request),
    }
}

/// Anthropic requires `max_tokens`, and Gemini requires it to be present for
/// some vendors, so supply a generous default when the caller omitted it.
fn effective_max_tokens(request: &CanonicalRequest) -> u32 {
    request
        .max_output_tokens
        .unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS)
}

// ─── OpenAI Chat Completions ───

fn encode_openai_chat(request: &CanonicalRequest) -> Value {
    let mut messages = Vec::new();

    // Chat Completions has no system field, so the system blocks lead the list.
    for block in &request.system {
        if let ContentBlock::Text { text } = block {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    for message in &request.messages {
        let text: Vec<&str> = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let joined = text.join("");
        let tool_results: Vec<&ContentBlock> = message
            .content
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
            .collect();
        let tool_uses: Vec<&ContentBlock> = message
            .content
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
            .collect();

        if message.role == Role::User {
            // Each tool result is its own `role: "tool"` message.
            for block in tool_results {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = block
                {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": content,
                    }));
                }
            }
            if !joined.is_empty() {
                messages.push(chat_user_content(&joined, &message.content));
            }
        } else {
            let mut assistant = Map::new();
            assistant.insert("role".to_string(), json!("assistant"));
            assistant.insert("content".to_string(), json!(joined));
            if !tool_uses.is_empty() {
                let calls: Vec<Value> = tool_uses
                    .iter()
                    .filter_map(|block| {
                        let ContentBlock::ToolUse { id, name, input } = block else {
                            return None;
                        };
                        Some(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string()),
                            }
                        }))
                    })
                    .collect();
                assistant.insert("tool_calls".to_string(), Value::Array(calls));
            }
            messages.push(Value::Object(assistant));
        }
    }

    let mut body = Map::new();
    body.insert("model".to_string(), json!(request.model));
    body.insert("messages".to_string(), Value::Array(messages));
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }
                })
            })
            .collect();
        body.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(max) = request.max_output_tokens {
        body.insert("max_tokens".to_string(), json!(max));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".to_string(), json!(top_p));
    }
    if !request.stop_sequences.is_empty() {
        body.insert("stop".to_string(), json!(request.stop_sequences));
    }
    if request.stream {
        body.insert("stream".to_string(), json!(true));
        // Ask for usage in the final chunk so token counts survive the trip.
        body.insert("stream_options".to_string(), json!({"include_usage": true}));
    }
    if let Some(thinking) = request.thinking {
        let effort = effort_for_budget(thinking.budget_tokens);
        body.insert("reasoning_effort".to_string(), json!(effort));
    }
    Value::Object(body)
}

/// A user turn carrying images needs the array form; text-only turns use the
/// compact string form that most vendors prefer.
fn chat_user_content(joined: &str, content: &[ContentBlock]) -> Value {
    let has_image = content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { .. }));
    if !has_image {
        return json!(joined);
    }
    let mut parts = Vec::new();
    if !joined.is_empty() {
        parts.push(json!({"type": "text", "text": joined}));
    }
    for block in content {
        if let ContentBlock::Image { media_type, data } = block {
            parts.push(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{media_type};base64,{data}")},
            }));
        }
    }
    Value::Array(parts)
}

fn effort_for_budget(budget: u32) -> &'static str {
    if budget <= 2_048 {
        "low"
    } else if budget >= 16_384 {
        "high"
    } else {
        "medium"
    }
}

// ─── OpenAI Responses ───

fn encode_openai_responses(request: &CanonicalRequest) -> Value {
    let mut input = Vec::new();

    for message in &request.messages {
        match message.role {
            Role::Assistant => {
                let text: String = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .concat();
                if !text.is_empty() {
                    input.push(json!({
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for block in &message.content {
                    if let ContentBlock::ToolUse {
                        id,
                        name,
                        input: args,
                    } = block
                    {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string()),
                        }));
                    }
                }
            }
            Role::User => {
                // Tool results are standalone items, not message content.
                for block in &message.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": content,
                        }));
                    }
                }
                let mut parts = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            parts.push(json!({"type": "input_text", "text": text}))
                        }
                        ContentBlock::Image { media_type, data } => parts.push(json!({
                            "type": "input_image",
                            "image_url": format!("data:{media_type};base64,{data}"),
                        })),
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    input.push(json!({"role": "user", "content": parts}));
                }
            }
        }
    }

    let mut body = Map::new();
    body.insert("model".to_string(), json!(request.model));
    let instructions = join_text(&request.system);
    if !instructions.is_empty() {
        body.insert("instructions".to_string(), json!(instructions));
    }
    body.insert("input".to_string(), Value::Array(input));
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect();
        body.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(max) = request.max_output_tokens {
        body.insert("max_output_tokens".to_string(), json!(max));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".to_string(), json!(top_p));
    }
    if request.stream {
        body.insert("stream".to_string(), json!(true));
    }
    if let Some(thinking) = request.thinking {
        body.insert(
            "reasoning".to_string(),
            json!({"effort": effort_for_budget(thinking.budget_tokens), "summary": "auto"}),
        );
    }
    Value::Object(body)
}

// ─── Anthropic Messages ───

fn encode_anthropic(request: &CanonicalRequest) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| {
            json!({
                "role": match message.role {
                    Role::Assistant => "assistant",
                    Role::User => "user",
                },
                "content": anthropic_content(&message.content),
            })
        })
        .collect();

    let mut body = Map::new();
    body.insert("model".to_string(), json!(request.model));
    let system = anthropic_content(&request.system);
    if system != Value::Array(vec![]) {
        body.insert("system".to_string(), system);
    }
    body.insert("messages".to_string(), Value::Array(messages));
    // Anthropic has no `stop`, it uses `stop_sequences`.
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
        body.insert("tools".to_string(), Value::Array(tools));
    }
    body.insert(
        "max_tokens".to_string(),
        json!(effective_max_tokens(request)),
    );
    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".to_string(), json!(top_p));
    }
    if !request.stop_sequences.is_empty() {
        body.insert("stop_sequences".to_string(), json!(request.stop_sequences));
    }
    if request.stream {
        body.insert("stream".to_string(), json!(true));
    }
    if let Some(thinking) = request.thinking {
        body.insert(
            "thinking".to_string(),
            json!({"type": "enabled", "budget_tokens": thinking.budget_tokens}),
        );
    }
    Value::Object(body)
}

fn anthropic_content(content: &[ContentBlock]) -> Value {
    // Every block variant has an Anthropic representation, so this is a total
    // map rather than a fallible one.
    let parts: Vec<Value> = content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => json!({"type": "text", "text": text}),
            ContentBlock::Image { media_type, data } => json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data},
            }),
            ContentBlock::ToolUse { id, name, input } => json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            }),
            ContentBlock::Thinking { text, signature } => json!({
                "type": "thinking",
                "thinking": text,
                "signature": signature.clone().unwrap_or_default(),
            }),
        })
        .collect();
    Value::Array(parts)
}

// ─── Gemini generateContent ───

fn encode_gemini(request: &CanonicalRequest) -> Value {
    // Gemini addresses a functionResponse by NAME, so resolve each tool-result
    // id back to the name of the call it answers.
    let mut names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for message in &request.messages {
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                names.insert(id.as_str(), name.as_str());
            }
        }
    }

    let contents: Vec<Value> = request
        .messages
        .iter()
        .map(|message| {
            let mut parts = Vec::new();
            for block in &message.content {
                match block {
                    ContentBlock::Text { text } => parts.push(json!({"text": text})),
                    ContentBlock::Image { media_type, data } => parts.push(json!({
                        "inlineData": {"mimeType": media_type, "data": data}
                    })),
                    ContentBlock::ToolUse { name, input, .. } => {
                        parts.push(json!({"functionCall": {"name": name, "args": input}}))
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        let name = names
                            .get(tool_use_id.as_str())
                            .copied()
                            .unwrap_or(tool_use_id);
                        parts.push(json!({
                            "functionResponse": {
                                "name": name,
                                "response": {"result": content},
                            }
                        }));
                    }
                    // Gemini has no first-class thinking block in the request;
                    // replayed reasoning is dropped rather than mis-encoded.
                    ContentBlock::Thinking { .. } => {}
                }
            }
            json!({
                "role": match message.role {
                    Role::Assistant => "model",
                    Role::User => "user",
                },
                "parts": parts,
            })
        })
        .collect();

    let mut body = Map::new();
    body.insert("contents".to_string(), Value::Array(contents));

    let system_text = join_text(&request.system);
    if !system_text.is_empty() {
        body.insert(
            "systemInstruction".to_string(),
            json!({"parts": [{"text": system_text}]}),
        );
    }

    if !request.tools.is_empty() {
        let declarations: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect();
        body.insert(
            "tools".to_string(),
            json!([{"functionDeclarations": declarations}]),
        );
    }

    let mut config = Map::new();
    config.insert(
        "maxOutputTokens".to_string(),
        json!(effective_max_tokens(request)),
    );
    if let Some(temperature) = request.temperature {
        config.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        config.insert("topP".to_string(), json!(top_p));
    }
    if !request.stop_sequences.is_empty() {
        config.insert("stopSequences".to_string(), json!(request.stop_sequences));
    }
    if let Some(thinking) = request.thinking {
        config.insert(
            "thinkingConfig".to_string(),
            json!({"thinkingBudget": thinking.budget_tokens, "includeThoughts": true}),
        );
    }
    body.insert("generationConfig".to_string(), Value::Object(config));

    Value::Object(body)
}

/// Concatenate every text block in a block list.
fn join_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .concat()
}

/// Map a provider's finish reason onto the canonical enum.
pub fn stop_reason_from(raw: &str) -> StopReason {
    match raw {
        "stop" | "end_turn" | "STOP" | "COMPLETE" => StopReason::EndTurn,
        "length" | "max_tokens" | "MAX_TOKENS" => StopReason::MaxTokens,
        "tool_calls" | "tool_use" | "function_call" => StopReason::ToolUse,
        "stop_sequence" | "STOP_SEQUENCE" => StopReason::StopSequence,
        _ => StopReason::Other,
    }
}

/// Render a canonical stop reason in `protocol`'s vocabulary.
pub fn stop_reason_to(protocol: WireProtocol, reason: StopReason) -> &'static str {
    match protocol {
        WireProtocol::OpenaiChat => match reason {
            StopReason::EndTurn | StopReason::StopSequence | StopReason::Other => "stop",
            StopReason::MaxTokens => "length",
            StopReason::ToolUse => "tool_calls",
        },
        WireProtocol::OpenaiResponses => match reason {
            StopReason::EndTurn | StopReason::StopSequence | StopReason::Other => "completed",
            StopReason::MaxTokens => "incomplete",
            StopReason::ToolUse => "completed",
        },
        WireProtocol::AnthropicMessages => match reason {
            StopReason::EndTurn | StopReason::Other => "end_turn",
            StopReason::MaxTokens => "max_tokens",
            StopReason::ToolUse => "tool_use",
            StopReason::StopSequence => "stop_sequence",
        },
        WireProtocol::GeminiGenerateContent => match reason {
            StopReason::MaxTokens => "MAX_TOKENS",
            _ => "STOP",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::protocol::{CanonicalMessage, CanonicalTool};

    fn sample() -> CanonicalRequest {
        CanonicalRequest {
            model: "gpt-test".to_string(),
            system: vec![ContentBlock::text("be nice")],
            messages: vec![
                CanonicalMessage::user_text("hi"),
                CanonicalMessage {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::text("calling"),
                        ContentBlock::ToolUse {
                            id: "call_1".to_string(),
                            name: "lookup".to_string(),
                            input: serde_json::json!({"q": "x"}),
                        },
                    ],
                },
                CanonicalMessage {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: "42".to_string(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![CanonicalTool {
                name: "lookup".to_string(),
                description: "look things up".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            max_output_tokens: Some(256),
            temperature: Some(0.5),
            stream: true,
            ..Default::default()
        }
    }

    #[test]
    fn chat_encoding_promotes_system_and_splits_tool_results() {
        let body = encode_openai_chat(&sample());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be nice");
        // user text, assistant with tool_calls, then a role:"tool" result.
        let tool_message = messages
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("tool result message");
        assert_eq!(tool_message["tool_call_id"], "call_1");
        assert_eq!(tool_message["content"], "42");
        let assistant = messages
            .iter()
            .find(|message| message["role"] == "assistant")
            .unwrap();
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "lookup");
        assert_eq!(body["tools"][0]["function"]["name"], "lookup");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn responses_encoding_uses_flat_tools_and_standalone_outputs() {
        let body = encode_openai_responses(&sample());
        assert_eq!(body["instructions"], "be nice");
        // Responses nests the function directly on the tool.
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["tools"][0]["type"], "function");
        let input = body["input"].as_array().unwrap();
        let call = input
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap();
        assert_eq!(call["call_id"], "call_1");
        let output = input
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .unwrap();
        assert_eq!(output["output"], "42");
        assert_eq!(body["max_output_tokens"], 256);
    }

    #[test]
    fn anthropic_encoding_keeps_system_separate_and_supplies_max_tokens() {
        let mut request = sample();
        request.max_output_tokens = None;
        let body = encode_anthropic(&request);
        assert_eq!(body["system"][0]["text"], "be nice");
        // Required by the API even when the caller omitted it.
        assert_eq!(body["max_tokens"], ANTHROPIC_DEFAULT_MAX_TOKENS);
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        let tool_result = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap().iter())
            .find(|part| part["type"] == "tool_result")
            .unwrap();
        assert_eq!(tool_result["tool_use_id"], "call_1");
    }

    #[test]
    fn gemini_encoding_resolves_tool_result_ids_back_to_names() {
        let body = encode_gemini(&sample());
        let contents = body["contents"].as_array().unwrap();
        // Gemini names the assistant turn "model".
        assert_eq!(contents[0]["role"], "user");
        let call = contents
            .iter()
            .flat_map(|content| content["parts"].as_array().unwrap().iter())
            .find(|part| part["functionCall"].is_object())
            .unwrap();
        assert_eq!(call["functionCall"]["name"], "lookup");
        let response = contents
            .iter()
            .flat_map(|content| content["parts"].as_array().unwrap().iter())
            .find(|part| part["functionResponse"].is_object())
            .unwrap();
        // The canonical id "call_1" resolves back to the function name.
        assert_eq!(response["functionResponse"]["name"], "lookup");
        assert_eq!(response["functionResponse"]["response"]["result"], "42");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be nice");
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["name"],
            "lookup"
        );
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 256);
    }

    #[test]
    fn stop_reasons_map_both_directions() {
        for protocol in [
            WireProtocol::OpenaiChat,
            WireProtocol::OpenaiResponses,
            WireProtocol::AnthropicMessages,
            WireProtocol::GeminiGenerateContent,
        ] {
            for reason in [
                StopReason::EndTurn,
                StopReason::MaxTokens,
                StopReason::ToolUse,
                StopReason::StopSequence,
                StopReason::Other,
            ] {
                let raw = stop_reason_to(protocol, reason);
                // A round trip must land on a defined reason, never a blank.
                assert!(!raw.is_empty());
            }
        }
        assert_eq!(stop_reason_from("tool_calls"), StopReason::ToolUse);
        assert_eq!(stop_reason_from("length"), StopReason::MaxTokens);
        assert_eq!(stop_reason_from("MAX_TOKENS"), StopReason::MaxTokens);
        assert_eq!(stop_reason_from("mystery"), StopReason::Other);
    }

    #[test]
    fn effort_budgets_round_trip_through_both_protocols() {
        for budget in [1024u32, 8192, 24576] {
            let effort = effort_for_budget(budget);
            let back = match effort {
                "low" => 1024,
                "high" => 24_576,
                _ => 8_192,
            };
            assert_eq!(effort_for_budget(back), effort);
        }
    }
}
