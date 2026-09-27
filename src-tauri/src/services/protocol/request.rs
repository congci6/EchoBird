//! Inbound request parsing: every supported wire protocol -> [`CanonicalRequest`].
//!
//! The bridge receives whatever the calling client speaks, so parsing has to be
//! total: any field it does not understand is dropped rather than rejected, and
//! the model id is recovered even for Gemini (where it lives in the URL).

use super::{
    CanonicalMessage, CanonicalRequest, CanonicalTool, ContentBlock, Role, ThinkingConfig,
    WireProtocol,
};
use serde_json::Value;
use std::collections::HashMap;

/// Map a Responses-style reasoning effort onto an Anthropic thinking budget.
/// Anthropic requires an explicit token count, so the coarse OpenAI levels are
/// translated into budgets the vendor accepts.
fn budget_for_effort(effort: &str) -> u32 {
    match effort {
        "low" => 1_024,
        "high" => 24_576,
        _ => 8_192,
    }
}

/// Parse a body received in `protocol` into the canonical form.
///
/// `path_model` carries the model id Gemini embeds in the URL; it is ignored by
/// the other three, which read `model` from the body.
pub fn parse_request(
    protocol: WireProtocol,
    body: &Value,
    path_model: Option<&str>,
) -> Result<CanonicalRequest, String> {
    match protocol {
        WireProtocol::OpenaiChat => parse_openai_chat(body),
        WireProtocol::OpenaiResponses => parse_openai_responses(body),
        WireProtocol::AnthropicMessages => parse_anthropic(body),
        WireProtocol::GeminiGenerateContent => parse_gemini(body, path_model),
    }
}

// ─── OpenAI Chat Completions ───

fn parse_openai_chat(body: &Value) -> Result<CanonicalRequest, String> {
    let mut request = CanonicalRequest {
        model: string_at(body, "model").unwrap_or_default(),
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        max_output_tokens: body
            .get("max_completion_tokens")
            .or_else(|| body.get("max_tokens"))
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        ..Default::default()
    };

    // `stop` is a string or an array of strings depending on the vendor.
    match body.get("stop") {
        Some(Value::String(stop)) => request.stop_sequences.push(stop.clone()),
        Some(Value::Array(stops)) => {
            for stop in stops.iter().filter_map(Value::as_str) {
                request.stop_sequences.push(stop.to_string());
            }
        }
        _ => {}
    }

    if let Some(effort) = body.get("reasoning_effort").and_then(Value::as_str) {
        request.thinking = Some(ThinkingConfig {
            budget_tokens: budget_for_effort(effort),
        });
    }

    for tool in array_at(body, "tools") {
        let Some(function) = tool.get("function") else {
            continue;
        };
        let Some(name) = function.get("name").and_then(Value::as_str) else {
            continue;
        };
        request.tools.push(CanonicalTool {
            name: name.to_string(),
            description: function
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            parameters: function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type": "object"})),
        });
    }

    for message in array_at(body, "messages") {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "system" | "developer" => {
                push_text_blocks(&mut request.system, message.get("content"));
            }
            "tool" | "function" => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let content = message.get("content").map(flatten_text).unwrap_or_default();
                request.messages.push(CanonicalMessage {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: id,
                        content,
                        is_error: false,
                    }],
                });
            }
            "assistant" => {
                let mut blocks = Vec::new();
                push_text_blocks(&mut blocks, message.get("content"));
                // A refusal is a normal assistant turn, not an error.
                if let Some(refusal) = message.get("refusal").and_then(Value::as_str) {
                    if !refusal.is_empty() {
                        blocks.push(ContentBlock::text(refusal));
                    }
                }
                for call in array_at(message, "tool_calls") {
                    let function = call.get("function");
                    let Some(name) = function.and_then(|f| f.get("name")).and_then(Value::as_str)
                    else {
                        continue;
                    };
                    let arguments = function
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    blocks.push(ContentBlock::ToolUse {
                        id: call
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: name.to_string(),
                        input: serde_json::from_str(arguments)
                            .unwrap_or_else(|_| serde_json::json!({})),
                    });
                }
                if !blocks.is_empty() {
                    request.messages.push(CanonicalMessage {
                        role: Role::Assistant,
                        content: blocks,
                    });
                }
            }
            _ => {
                let mut blocks = Vec::new();
                push_chat_user_blocks(&mut blocks, message.get("content"));
                if !blocks.is_empty() {
                    request.messages.push(CanonicalMessage {
                        role: Role::User,
                        content: blocks,
                    });
                }
            }
        }
    }

    Ok(request)
}

/// Chat Completions user content is either a bare string or a list of
/// `{"type": "text" | "image_url"}` parts.
fn push_chat_user_blocks(blocks: &mut Vec<ContentBlock>, content: Option<&Value>) {
    let Some(content) = content else {
        return;
    };
    if let Some(text) = content.as_str() {
        if !text.is_empty() {
            blocks.push(ContentBlock::text(text));
        }
        return;
    }
    for part in array_ref(content) {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        blocks.push(ContentBlock::text(text));
                    }
                }
            }
            Some("image_url") => {
                let url = part
                    .get("image_url")
                    .and_then(|image| image.get("url"))
                    .and_then(Value::as_str)
                    .or_else(|| part.get("image_url").and_then(Value::as_str));
                if let Some((media_type, data)) = url.and_then(split_data_url) {
                    blocks.push(ContentBlock::Image {
                        media_type,
                        data: data.to_string(),
                    });
                }
            }
            // A file part carries base64 inline; keep the common image case.
            Some("file") => {
                if let Some(file) = part.get("file") {
                    if let Some(data) = file.get("file_data").and_then(Value::as_str) {
                        let media_type = file
                            .get("media_type")
                            .and_then(Value::as_str)
                            .unwrap_or("image/png")
                            .to_string();
                        let data = data
                            .strip_prefix("data:")
                            .and_then(|rest| rest.split_once(";base64,"))
                            .map(|(_, encoded)| encoded.to_string())
                            .unwrap_or_else(|| data.to_string());
                        blocks.push(ContentBlock::Image {
                            media_type,
                            data: data.to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

// ─── OpenAI Responses ───

fn parse_openai_responses(body: &Value) -> Result<CanonicalRequest, String> {
    let mut request = CanonicalRequest {
        model: string_at(body, "model").unwrap_or_default(),
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        max_output_tokens: body
            .get("max_output_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        ..Default::default()
    };

    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        if !instructions.is_empty() {
            request.system.push(ContentBlock::text(instructions));
        }
    }

    for tool in array_at(body, "tools") {
        // Responses flattens the function fields onto the tool object itself.
        if tool.get("type").and_then(Value::as_str) == Some("function")
            || tool.get("name").is_some()
        {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            request.tools.push(CanonicalTool {
                name: name.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                parameters: tool
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object"})),
            });
        }
    }

    if let Some(effort) = body
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
    {
        request.thinking = Some(ThinkingConfig {
            budget_tokens: budget_for_effort(effort),
        });
    }

    for item in array_at(body, "input") {
        let item_type = item.get("type").and_then(Value::as_str);
        match item_type {
            Some("function_call") => {
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                request.messages.push(CanonicalMessage {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
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
                        input: serde_json::from_str(arguments)
                            .unwrap_or_else(|_| serde_json::json!({})),
                    }],
                });
            }
            Some("function_call_output") => {
                let output = item.get("output").map(flatten_text).unwrap_or_default();
                request.messages.push(CanonicalMessage {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        content: output,
                        is_error: false,
                    }],
                });
            }
            Some("reasoning") => {
                // Replayed reasoning items are informational; preserving the
                // text keeps multi-turn tool loops coherent.
                if let Some(summary) = item.get("summary").map(flatten_text) {
                    if !summary.is_empty() {
                        request.messages.push(CanonicalMessage {
                            role: Role::Assistant,
                            content: vec![ContentBlock::Thinking {
                                text: summary,
                                signature: None,
                            }],
                        });
                    }
                }
            }
            _ => {
                let role = match item.get("role").and_then(Value::as_str) {
                    Some("assistant") => Role::Assistant,
                    _ => Role::User,
                };
                let mut blocks = Vec::new();
                push_responses_content(&mut blocks, item.get("content"));
                if !blocks.is_empty() {
                    request.messages.push(CanonicalMessage {
                        role,
                        content: blocks,
                    });
                }
            }
        }
    }

    Ok(request)
}

fn push_responses_content(blocks: &mut Vec<ContentBlock>, content: Option<&Value>) {
    let Some(content) = content else {
        return;
    };
    if let Some(text) = content.as_str() {
        if !text.is_empty() {
            blocks.push(ContentBlock::text(text));
        }
        return;
    }
    for part in array_ref(content) {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("output_text") | Some("text") | Some("summary_text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        blocks.push(ContentBlock::text(text));
                    }
                }
            }
            Some("input_image") => {
                let url = part
                    .get("image_url")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        part.get("image_url")
                            .and_then(|image| image.get("url"))
                            .and_then(Value::as_str)
                    })
                    .or_else(|| part.get("file_id").and_then(Value::as_str));
                // `file_id` references a previously uploaded file we cannot
                // re-read, so it is skipped rather than sent as broken bytes.
                if let Some((media_type, data)) = url.and_then(split_data_url) {
                    blocks.push(ContentBlock::Image {
                        media_type,
                        data: data.to_string(),
                    });
                }
            }
            Some("refusal") => {
                if let Some(text) = part.get("refusal").and_then(Value::as_str) {
                    if !text.is_empty() {
                        blocks.push(ContentBlock::text(text));
                    }
                }
            }
            _ => {}
        }
    }
}

// ─── Anthropic Messages ───

fn parse_anthropic(body: &Value) -> Result<CanonicalRequest, String> {
    let mut request = CanonicalRequest {
        model: string_at(body, "model").unwrap_or_default(),
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        max_output_tokens: body
            .get("max_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as u32),
        temperature: body.get("temperature").and_then(Value::as_f64),
        top_p: body.get("top_p").and_then(Value::as_f64),
        ..Default::default()
    };

    if let Some(system) = body.get("system") {
        push_anthropic_blocks(&mut request.system, Some(system));
    }

    for stop in array_at(body, "stop_sequences") {
        if let Some(stop) = stop.as_str() {
            request.stop_sequences.push(stop.to_string());
        }
    }

    if let Some(thinking) = body.get("thinking") {
        if thinking.get("type").and_then(Value::as_str) == Some("enabled") {
            request.thinking = Some(ThinkingConfig {
                budget_tokens: thinking
                    .get("budget_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(4_096) as u32,
            });
        }
    }

    for tool in array_at(body, "tools") {
        // Server-side tools (web_search, bash, …) have no input schema and are
        // not portable across protocols, so only plain functions are carried.
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        if tool
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "custom")
        {
            continue;
        }
        request.tools.push(CanonicalTool {
            name: name.to_string(),
            description: tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            parameters: tool
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type": "object"})),
        });
    }

    for message in array_at(body, "messages") {
        let role = match message.get("role").and_then(Value::as_str) {
            Some("assistant") => Role::Assistant,
            _ => Role::User,
        };
        let mut blocks = Vec::new();
        push_anthropic_blocks(&mut blocks, message.get("content"));
        if !blocks.is_empty() {
            request.messages.push(CanonicalMessage {
                role,
                content: blocks,
            });
        }
    }

    Ok(request)
}

fn push_anthropic_blocks(blocks: &mut Vec<ContentBlock>, content: Option<&Value>) {
    let Some(content) = content else {
        return;
    };
    if let Some(text) = content.as_str() {
        if !text.is_empty() {
            blocks.push(ContentBlock::text(text));
        }
        return;
    }
    for part in array_ref(content) {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        blocks.push(ContentBlock::text(text));
                    }
                }
            }
            Some("thinking") => {
                if let Some(text) = part.get("thinking").and_then(Value::as_str) {
                    blocks.push(ContentBlock::Thinking {
                        text: text.to_string(),
                        signature: part
                            .get("signature")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    });
                }
            }
            Some("redacted_thinking") => {
                blocks.push(ContentBlock::Thinking {
                    text: String::new(),
                    signature: part.get("data").and_then(Value::as_str).map(str::to_string),
                });
            }
            Some("image") => {
                let source = part.get("source");
                let media_type = source
                    .and_then(|source| source.get("media_type"))
                    .and_then(Value::as_str)
                    .unwrap_or("image/png")
                    .to_string();
                if let Some(data) = source
                    .and_then(|source| source.get("data"))
                    .and_then(Value::as_str)
                {
                    blocks.push(ContentBlock::Image {
                        media_type,
                        data: data.to_string(),
                    });
                }
            }
            Some("tool_use") => {
                blocks.push(ContentBlock::ToolUse {
                    id: part
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: part
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    input: part
                        .get("input")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                });
            }
            Some("tool_result") => {
                let inner = part.get("content");
                let is_error = part
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                blocks.push(ContentBlock::ToolResult {
                    tool_use_id: part
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    // Anthropic allows structured tool output; flatten it so
                    // protocols limited to a string still get the payload.
                    content: inner.map(flatten_text).unwrap_or_default(),
                    is_error,
                });
            }
            _ => {}
        }
    }
}

// ─── Gemini generateContent ───

fn parse_gemini(body: &Value, path_model: Option<&str>) -> Result<CanonicalRequest, String> {
    let mut request = CanonicalRequest {
        model: path_model
            .map(str::to_string)
            .or_else(|| string_at(body, "model"))
            .unwrap_or_default(),
        stream: false,
        ..Default::default()
    };

    if let Some(system) = body
        .get("systemInstruction")
        .or_else(|| body.get("system_instruction"))
    {
        let mut blocks = Vec::new();
        for part in system
            .get("parts")
            .map(array_ref)
            .unwrap_or_default()
            .iter()
        {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    blocks.push(ContentBlock::text(text));
                }
            }
        }
        if blocks.is_empty() {
            push_text_blocks(&mut blocks, system.get("text"));
        }
        request.system = blocks;
    }

    // Gemini hoists sampling parameters into generationConfig.
    if let Some(config) = body.get("generationConfig") {
        request.max_output_tokens = config
            .get("maxOutputTokens")
            .and_then(Value::as_u64)
            .map(|value| value as u32);
        request.temperature = config.get("temperature").and_then(Value::as_f64);
        request.top_p = config.get("topP").and_then(Value::as_f64);
        for stop in array_at(config, "stopSequences") {
            if let Some(stop) = stop.as_str() {
                request.stop_sequences.push(stop.to_string());
            }
        }
        if let Some(thinking) = config.get("thinkingConfig") {
            let budget = thinking
                .get("thinkingBudget")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            // A negative budget means "disable thinking" in the Gemini API.
            if budget > 0 {
                request.thinking = Some(ThinkingConfig {
                    budget_tokens: budget.min(u32::MAX as i64) as u32,
                });
            }
        }
    }

    for tool in array_at(body, "tools") {
        for declaration in array_at(tool, "functionDeclarations") {
            let Some(name) = declaration.get("name").and_then(Value::as_str) else {
                continue;
            };
            request.tools.push(CanonicalTool {
                name: name.to_string(),
                description: declaration
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                parameters: declaration
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object"})),
            });
        }
    }

    // Gemini correlates a functionResponse to its call by NAME, not by id, so
    // ids are minted here and the name->id mapping is kept for encoding.
    let mut call_ids: HashMap<String, String> = HashMap::new();
    let mut sequence = 0u32;

    for content in array_at(body, "contents") {
        let role = match content.get("role").and_then(Value::as_str) {
            Some("model") => Role::Assistant,
            _ => Role::User,
        };
        let mut blocks = Vec::new();
        for part in array_at(content, "parts") {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    // `thought: true` marks Gemini's reasoning summary.
                    if part.get("thought").and_then(Value::as_bool) == Some(true) {
                        blocks.push(ContentBlock::Thinking {
                            text: text.to_string(),
                            signature: None,
                        });
                    } else {
                        blocks.push(ContentBlock::text(text));
                    }
                }
            }
            if let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data")) {
                if let Some(data) = inline.get("data").and_then(Value::as_str) {
                    blocks.push(ContentBlock::Image {
                        media_type: inline
                            .get("mimeType")
                            .or_else(|| inline.get("mime_type"))
                            .and_then(Value::as_str)
                            .unwrap_or("image/png")
                            .to_string(),
                        data: data.to_string(),
                    });
                }
            }
            if let Some(call) = part
                .get("functionCall")
                .or_else(|| part.get("function_call"))
            {
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let id = format!("call_{sequence}");
                sequence += 1;
                call_ids.entry(name.clone()).or_insert(id.clone());
                blocks.push(ContentBlock::ToolUse {
                    id,
                    name,
                    input: call
                        .get("args")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                });
            }
            if let Some(response) = part
                .get("functionResponse")
                .or_else(|| part.get("function_response"))
            {
                let name = response
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let id = call_ids.get(&name).cloned().unwrap_or_else(|| {
                    let id = format!("call_{sequence}");
                    sequence += 1;
                    id
                });
                blocks.push(ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: response
                        .get("response")
                        .map(flatten_text)
                        .unwrap_or_default(),
                    is_error: false,
                });
            }
        }
        if !blocks.is_empty() {
            request.messages.push(CanonicalMessage {
                role,
                content: blocks,
            });
        }
    }

    Ok(request)
}

// ─── Shared helpers ───

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn array_at<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn array_ref(value: &Value) -> Vec<&Value> {
    value
        .as_array()
        .map(|items| items.iter().collect())
        .unwrap_or_default()
}

/// Collapse a string-or-array content field into plain text. Nested objects
/// (tool results, function payloads) are stringified rather than dropped, so a
/// structured upstream payload survives a trip through a text-only protocol.
fn flatten_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Array(items) => items.iter().map(flatten_text).collect(),
        Value::Object(map) => {
            // Anthropic tool results wrap their text in a block list.
            if let Some(content) = map.get("content") {
                return flatten_text(content);
            }
            serde_json::to_string(value).unwrap_or_default()
        }
        other => other.to_string(),
    }
}

fn push_text_blocks(blocks: &mut Vec<ContentBlock>, content: Option<&Value>) {
    let Some(content) = content else {
        return;
    };
    if let Some(text) = content.as_str() {
        if !text.is_empty() {
            blocks.push(ContentBlock::text(text));
        }
        return;
    }
    for part in array_ref(content) {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                blocks.push(ContentBlock::text(text));
            }
        }
    }
}

/// Split a `data:<mime>;base64,<payload>` URL into its parts. Remote `http(s)`
/// URLs return `None`: EchoBird does not fetch third-party images on the
/// caller's behalf.
fn split_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, payload) = rest.split_once(";base64,")?;
    Some((media_type.to_string(), payload.to_string()))
}
