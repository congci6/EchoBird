//! Streaming conversion in both directions.
//!
//! [`StreamDecoder`] turns an upstream SSE byte stream into [`StreamEvent`]s;
//! [`StreamEncoder`] renders those events as SSE in the caller's protocol.
//! The two are independent, so any upstream protocol can feed any client
//! protocol — including a Gemini stream replayed to an Anthropic client.
//!
//! Both are stateful because three of the four protocols are not a flat list
//! of deltas: Anthropic brackets content with `content_block_start`/`_stop` and
//! allows only one open block at a time, and Responses models output as
//! indexed items that must be opened and closed in order.

use super::{
    now_seconds, stop_reason_from, stop_reason_to, StopReason, StreamEvent, Usage, WireProtocol,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// A tool call being assembled from streaming argument deltas.
#[derive(Debug, Default, Clone)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

// ─── Decoder: upstream SSE -> StreamEvent ───

/// Normalizes one upstream SSE stream into canonical events.
pub struct StreamDecoder {
    protocol: WireProtocol,
    /// Partial line carried between network chunks.
    buffer: String,
    /// `event:` name seen most recently; Anthropic and Responses use it to
    /// disambiguate, the other two leave it empty.
    event_name: String,
    /// Tool calls already announced, so `ToolCallStart` fires exactly once.
    announced: BTreeMap<u32, PendingCall>,
    usage: Usage,
    /// Whether a terminal event was already emitted.
    finished: bool,
    /// Stop reason seen upstream. Emission is deferred to `finish` so a `Usage`
    /// event always precedes the terminal frame.
    stop_reason: Option<StopReason>,
}

impl StreamDecoder {
    pub fn new(protocol: WireProtocol) -> Self {
        Self {
            protocol,
            buffer: String::new(),
            event_name: String::new(),
            announced: BTreeMap::new(),
            usage: Usage::default(),
            finished: false,
            stop_reason: None,
        }
    }

    /// Feed the next network chunk and return whatever events completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<StreamEvent> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut events = Vec::new();
        while let Some(newline) = self.buffer.find('\n') {
            let line: String = self.buffer[..newline].trim_end_matches('\r').to_string();
            self.buffer.drain(..=newline);
            events.extend(self.line(&line));
        }
        events
    }

    /// Flush any trailing partial line and synthesize a terminal event when the
    /// upstream ended without an explicit one.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if !self.buffer.trim().is_empty() {
            let line = std::mem::take(&mut self.buffer)
                .trim_end_matches('\r')
                .to_string();
            events.extend(self.line(&line));
        }
        // Argument deltas were already emitted as they arrived, so nothing is
        // re-sent here: replaying the accumulated string would duplicate the
        // payload and leave the client holding unparseable JSON. All that is
        // left is to guarantee a terminal event.
        //
        // Usage precedes the terminal event, and the stop is emitted here even
        // when the upstream already signalled one, so the ordering holds no
        // matter where the stream ended.
        if self.usage.output_tokens > 0 {
            events.push(StreamEvent::Usage { usage: self.usage });
        }
        events.push(StreamEvent::Stop {
            reason: self.stop_reason.unwrap_or(StopReason::EndTurn),
        });
        events
    }

    fn line(&mut self, line: &str) -> Vec<StreamEvent> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        if let Some(name) = trimmed.strip_prefix("event:") {
            self.event_name = name.trim().to_string();
            return Vec::new();
        }
        let Some(payload) = trimmed.strip_prefix("data:") else {
            // Comment / retry / unknown field.
            return Vec::new();
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            return self.terminate();
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return Vec::new();
        };
        let event_name = std::mem::take(&mut self.event_name);
        match self.protocol {
            WireProtocol::OpenaiChat => self.decode_chat(&value),
            WireProtocol::OpenaiResponses => self.decode_responses(&value, &event_name),
            WireProtocol::AnthropicMessages => self.decode_anthropic(&value, &event_name),
            WireProtocol::GeminiGenerateContent => self.decode_gemini(&value),
        }
    }

    /// Record that the stream is over. The matching `Stop` is emitted by
    /// `finish`, never inline.
    fn record_stop(&mut self, reason: StopReason) {
        self.finished = true;
        self.stop_reason = Some(reason);
    }

    fn terminate(&mut self) -> Vec<StreamEvent> {
        if self.finished {
            return Vec::new();
        }
        self.record_stop(StopReason::EndTurn);
        Vec::new()
    }
}

impl StreamDecoder {
    fn decode_chat(&mut self, value: &Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if let Some(raw) = value.get("usage").filter(|usage| !usage.is_null()) {
            if let Some(prompt) = raw.get("prompt_tokens").and_then(Value::as_u64) {
                self.usage.input_tokens = prompt;
            }
            if let Some(completion) = raw.get("completion_tokens").and_then(Value::as_u64) {
                self.usage.output_tokens = completion;
            }
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return events;
        };
        if let Some(delta) = choice.get("delta") {
            // `reasoning_content` is the de-facto field for Chat-compatible
            // thinking models (DeepSeek, Qwen and friends).
            if let Some(thinking) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
            {
                if !thinking.is_empty() {
                    events.push(StreamEvent::ThinkingDelta {
                        text: thinking.to_string(),
                    });
                }
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    events.push(StreamEvent::TextDelta {
                        text: text.to_string(),
                    });
                }
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                let already_announced = self.announced.contains_key(&index);
                let entry = self.announced.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    entry.id = id.to_string();
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        entry.name = name.to_string();
                    }
                }
                // The opening frame must precede any argument delta: an
                // encoder buffers arguments against the announced call, so a
                // delta that arrives first has nothing to attach to.
                if !already_announced {
                    let snapshot = self.announced.get(&index).cloned().unwrap_or_default();
                    events.push(StreamEvent::ToolCallStart {
                        index,
                        id: if snapshot.id.is_empty() {
                            format!("call_{index}")
                        } else {
                            snapshot.id
                        },
                        name: snapshot.name,
                    });
                }
                if let Some(arguments) = call
                    .get("function")
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                {
                    if !arguments.is_empty() {
                        if let Some(call) = self.announced.get_mut(&index) {
                            call.arguments.push_str(arguments);
                        }
                        events.push(StreamEvent::ToolCallDelta {
                            index,
                            arguments: arguments.to_string(),
                        });
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.record_stop(stop_reason_from(reason));
        }
        events
    }

    fn decode_responses(&mut self, value: &Value, event_name: &str) -> Vec<StreamEvent> {
        // The payload repeats the event name in `type`; prefer whichever is set.
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event_name);
        let mut events = Vec::new();
        match kind {
            "response.output_text.delta" => {
                if let Some(text) = value.get("delta").and_then(Value::as_str) {
                    events.push(StreamEvent::TextDelta {
                        text: text.to_string(),
                    });
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = value.get("delta").and_then(Value::as_str) {
                    events.push(StreamEvent::ThinkingDelta {
                        text: text.to_string(),
                    });
                }
            }
            "response.output_item.added" => {
                let Some(item) = value.get("item") else {
                    return events;
                };
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let index = responses_item_index(Some(item));
                    let id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.announced.insert(
                        index,
                        PendingCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: String::new(),
                        },
                    );
                    events.push(StreamEvent::ToolCallStart { index, id, name });
                }
            }
            "response.function_call_arguments.delta" => {
                let index = responses_item_index(value.get("item_id"));
                if let Some(text) = value.get("delta").and_then(Value::as_str) {
                    if !text.is_empty() {
                        if let Some(call) = self.announced.get_mut(&index) {
                            call.arguments.push_str(text);
                        }
                        events.push(StreamEvent::ToolCallDelta {
                            index,
                            arguments: text.to_string(),
                        });
                    }
                }
            }
            "response.output_item.done" => {
                // A whole-item arrival carries the complete argument string.
                let Some(item) = value.get("item") else {
                    return events;
                };
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let index = responses_item_index(Some(item));
                    if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                        let missing = self
                            .announced
                            .get(&index)
                            .map(|call| call.arguments.is_empty())
                            .unwrap_or(true);
                        if !arguments.is_empty() && missing {
                            if let Some(call) = self.announced.get_mut(&index) {
                                call.arguments = arguments.to_string();
                            }
                            events.push(StreamEvent::ToolCallDelta {
                                index,
                                arguments: arguments.to_string(),
                            });
                        }
                    }
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                if let Some(raw) = value
                    .get("response")
                    .and_then(|response| response.get("usage"))
                {
                    self.usage = Usage {
                        input_tokens: raw.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                        output_tokens: raw
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    };
                }
                self.record_stop(if kind == "response.incomplete" {
                    StopReason::MaxTokens
                } else {
                    StopReason::EndTurn
                });
            }
            _ => {}
        }
        events
    }

    fn decode_anthropic(&mut self, value: &Value, event_name: &str) -> Vec<StreamEvent> {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event_name);
        let mut events = Vec::new();
        match kind {
            "content_block_start" => {
                let Some(block) = value.get("content_block") else {
                    return events;
                };
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.announced.insert(
                        index,
                        PendingCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: String::new(),
                        },
                    );
                    events.push(StreamEvent::ToolCallStart { index, id, name });
                }
            }
            "content_block_delta" => {
                let Some(delta) = value.get("delta") else {
                    return events;
                };
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            events.push(StreamEvent::TextDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            events.push(StreamEvent::ThinkingDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                            if !partial.is_empty() {
                                if let Some(call) = self.announced.get_mut(&index) {
                                    call.arguments.push_str(partial);
                                }
                                events.push(StreamEvent::ToolCallDelta {
                                    index,
                                    arguments: partial.to_string(),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(output) = value
                    .get("usage")
                    .and_then(|usage| usage.get("output_tokens"))
                    .and_then(Value::as_u64)
                {
                    self.usage.output_tokens = output;
                }
                if let Some(reason) = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.record_stop(stop_reason_from(reason));
                }
            }
            // A stream that ends without a reason still has to terminate.
            "message_stop" if !self.finished => self.record_stop(StopReason::EndTurn),
            _ => {}
        }
        events
    }

    fn decode_gemini(&mut self, value: &Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if let Some(raw) = value.get("usageMetadata") {
            if let Some(prompt) = raw.get("promptTokenCount").and_then(Value::as_u64) {
                self.usage.input_tokens = prompt;
            }
            if let Some(completion) = raw.get("candidatesTokenCount").and_then(Value::as_u64) {
                self.usage.output_tokens = completion;
            }
        }
        let Some(candidate) = value
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
        else {
            return events;
        };
        for part in candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    if part.get("thought").and_then(Value::as_bool) == Some(true) {
                        events.push(StreamEvent::ThinkingDelta {
                            text: text.to_string(),
                        });
                    } else {
                        events.push(StreamEvent::TextDelta {
                            text: text.to_string(),
                        });
                    }
                }
            }
            if let Some(call) = part.get("functionCall") {
                let index = self.announced.len() as u32;
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let id = format!("call_{index}");
                let arguments = call
                    .get("args")
                    .and_then(|args| serde_json::to_string(args).ok())
                    .unwrap_or_else(|| "{}".to_string());
                self.announced.insert(
                    index,
                    PendingCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    },
                );
                // Gemini delivers a whole call in a single part, so the start
                // and the complete argument payload are emitted together.
                events.push(StreamEvent::ToolCallStart { index, id, name });
                events.push(StreamEvent::ToolCallDelta { index, arguments });
            }
        }
        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            self.record_stop(stop_reason_from(reason));
        }
        events
    }
}

/// Responses ids look like `fc_0` / `fc_abc_2`; the trailing number is the
/// output index we correlate argument deltas against.
fn responses_item_index(value: Option<&Value>) -> u32 {
    value
        .and_then(Value::as_str)
        .and_then(|id| id.rsplit('-').next())
        .and_then(|tail| tail.parse::<u32>().ok())
        .unwrap_or(0)
}

// ─── Encoder: StreamEvent -> client SSE ───

/// Renders canonical events as SSE in one protocol.
pub struct StreamEncoder {
    protocol: WireProtocol,
    model: String,
    id: String,
    created: u64,
    /// Anthropic allows at most one open content block, tracked here.
    next_block_index: u32,
    open_block: Option<(u32, BlockKind)>,
    /// Responses output-item bookkeeping.
    message_open: bool,
    text_open: bool,
    /// Calls announced by `ToolCallStart`, keyed by canonical index.
    calls: BTreeMap<u32, PendingCall>,
    /// All text seen so far; Responses and Gemini re-emit it in full at the end.
    text_buffer: String,
    usage: Usage,
    stopped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
}

impl StreamEncoder {
    pub fn new(protocol: WireProtocol, model: &str) -> Self {
        let stamp = now_seconds();
        Self {
            protocol,
            model: model.to_string(),
            id: format!("chatcmpl_{stamp:x}"),
            created: stamp,
            next_block_index: 0,
            open_block: None,
            message_open: false,
            text_open: false,
            calls: BTreeMap::new(),
            text_buffer: String::new(),
            usage: Usage::default(),
            stopped: false,
        }
    }

    /// The opening frames a protocol requires before any delta.
    pub fn start(&mut self) -> Vec<u8> {
        match self.protocol {
            WireProtocol::AnthropicMessages => frame(
                "message_start",
                json!({
                    "type": "message_start",
                    "message": {
                        "id": self.id,
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": self.model,
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                    }
                }),
            ),
            WireProtocol::OpenaiResponses => {
                let mut out = frame("response.created", self.envelope("in_progress", &[]));
                out.extend(frame(
                    "response.in_progress",
                    self.envelope("in_progress", &[]),
                ));
                out
            }
            // Chat Completions and Gemini have no preamble.
            WireProtocol::OpenaiChat | WireProtocol::GeminiGenerateContent => Vec::new(),
        }
    }

    /// Convert one event into SSE bytes.
    pub fn push(&mut self, event: &StreamEvent) -> Vec<u8> {
        match event {
            StreamEvent::TextDelta { text } => self.text_delta(text),
            StreamEvent::ThinkingDelta { text } => self.thinking_delta(text),
            StreamEvent::ToolCallStart { index, id, name } => {
                self.tool_call_start(*index, id, name)
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                self.tool_call_delta(*index, arguments)
            }
            StreamEvent::Usage { usage } => {
                self.usage = *usage;
                Vec::new()
            }
            StreamEvent::Stop { reason } => {
                let out = self.stop(*reason);
                self.stopped = true;
                out
            }
        }
    }

    /// Trailing frames. Idempotent, so a caller may invoke it defensively.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.stopped {
            Vec::new()
        } else {
            self.stop(StopReason::EndTurn)
        }
    }

    fn envelope(&self, status: &str, output: &[Value]) -> Value {
        json!({
            "type": "response",
            "id": self.id,
            "object": "response",
            "created_at": self.created,
            "model": self.model,
            "status": status,
            "output": output,
            "usage": {
                "input_tokens": self.usage.input_tokens,
                "output_tokens": self.usage.output_tokens,
                "total_tokens": self.usage.input_tokens + self.usage.output_tokens,
            }
        })
    }

    fn text_delta(&mut self, text: &str) -> Vec<u8> {
        self.text_buffer.push_str(text);
        match self.protocol {
            WireProtocol::OpenaiChat => self.chat_chunk(json!({"content": text}), Value::Null),
            WireProtocol::OpenaiResponses => {
                let mut out = self.open_responses_message();
                out.extend(frame(
                    "response.output_text.delta",
                    json!({
                        "type": "response.output_text.delta",
                        "item_id": format!("msg_{}", self.id),
                        "output_index": 0,
                        "content_index": 0,
                        "delta": text,
                    }),
                ));
                out
            }
            WireProtocol::AnthropicMessages => {
                let mut out = Vec::new();
                if let Some((_, kind)) = self.open_block {
                    if kind != BlockKind::Text {
                        out.extend(self.close_block());
                    }
                }
                if self.open_block.is_none() {
                    let index = self.next_block_index;
                    self.next_block_index += 1;
                    self.open_block = Some((index, BlockKind::Text));
                    out.extend(frame(
                        "content_block_start",
                        json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {"type": "text", "text": ""},
                        }),
                    ));
                }
                let index = self.open_block.map_or(0, |(index, _)| index);
                out.extend(frame(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": text},
                    }),
                ));
                out
            }
            WireProtocol::GeminiGenerateContent => data_frame(json!({"candidates": [{
                "content": {"role": "model", "parts": [{"text": text}]},
                "index": 0,
            }]})),
        }
    }

    fn thinking_delta(&mut self, text: &str) -> Vec<u8> {
        match self.protocol {
            // Chat Completions has no standard field; `reasoning_content` is
            // what the compatible vendors both emit and read.
            WireProtocol::OpenaiChat => {
                self.chat_chunk(json!({"reasoning_content": text}), Value::Null)
            }
            WireProtocol::OpenaiResponses => frame(
                "response.reasoning_summary_text.delta",
                json!({
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": format!("rs_{}", self.id),
                    "output_index": 0,
                    "summary_index": 0,
                    "delta": text,
                }),
            ),
            WireProtocol::AnthropicMessages => {
                let mut out = Vec::new();
                if let Some((_, kind)) = self.open_block {
                    if kind != BlockKind::Thinking {
                        out.extend(self.close_block());
                    }
                }
                if self.open_block.is_none() {
                    let index = self.next_block_index;
                    self.next_block_index += 1;
                    self.open_block = Some((index, BlockKind::Thinking));
                    out.extend(frame(
                        "content_block_start",
                        json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {"type": "thinking", "thinking": "", "signature": ""},
                        }),
                    ));
                }
                let index = self.open_block.map_or(0, |(index, _)| index);
                out.extend(frame(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "thinking_delta", "thinking": text},
                    }),
                ));
                out
            }
            // Gemini marks reasoning with `thought`; a Chat-compatible client
            // has nowhere to put it, so it is dropped rather than mislabelled.
            WireProtocol::GeminiGenerateContent => data_frame(json!({"candidates": [{
                "content": {"role": "model", "parts": [{"text": text, "thought": true}]},
                "index": 0,
            }]})),
        }
    }

    fn tool_call_start(&mut self, index: u32, id: &str, name: &str) -> Vec<u8> {
        self.calls.insert(
            index,
            PendingCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: String::new(),
            },
        );
        match self.protocol {
            WireProtocol::OpenaiChat => self.chat_chunk(
                json!({
                    "tool_calls": [{
                        "index": index,
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": ""},
                    }]
                }),
                Value::Null,
            ),
            WireProtocol::OpenaiResponses => frame(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {
                        "id": self.call_item_id(index),
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": "",
                        "status": "in_progress",
                    }
                }),
            ),
            WireProtocol::AnthropicMessages => {
                let mut out = self.close_block();
                let block_index = self.next_block_index;
                self.next_block_index += 1;
                self.open_block = Some((block_index, BlockKind::ToolUse));
                out.extend(frame(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": block_index,
                        "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                    }),
                ));
                out
            }
            // Gemini has no partial-call form; buffered until the arguments
            // are complete and flushed by the terminal frame.
            WireProtocol::GeminiGenerateContent => Vec::new(),
        }
    }

    fn tool_call_delta(&mut self, index: u32, arguments: &str) -> Vec<u8> {
        if let Some(call) = self.calls.get_mut(&index) {
            call.arguments.push_str(arguments);
        }
        match self.protocol {
            WireProtocol::OpenaiChat => self.chat_chunk(
                json!({"tool_calls": [{"index": index, "function": {"arguments": arguments}}]}),
                Value::Null,
            ),
            WireProtocol::OpenaiResponses => frame(
                "response.function_call_arguments.delta",
                json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": self.call_item_id(index),
                    "output_index": 0,
                    "delta": arguments,
                }),
            ),
            WireProtocol::AnthropicMessages => {
                let index = self.open_block.map_or(index, |(block, _)| block);
                frame(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "input_json_delta", "partial_json": arguments},
                    }),
                )
            }
            WireProtocol::GeminiGenerateContent => Vec::new(),
        }
    }

    fn call_item_id(&self, index: u32) -> String {
        format!("fc_{}_{index}", self.id)
    }

    fn chat_chunk(&self, delta: Value, finish_reason: Value) -> Vec<u8> {
        data_frame(json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
        }))
    }
}

impl StreamEncoder {
    fn open_responses_message(&mut self) -> Vec<u8> {
        if self.text_open {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.message_open {
            self.message_open = true;
            out.extend(frame(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {
                        "id": format!("msg_{}", self.id),
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": [],
                    }
                }),
            ));
            out.extend(frame(
                "response.content_part.added",
                json!({
                    "type": "response.content_part.added",
                    "item_id": format!("msg_{}", self.id),
                    "output_index": 0,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []}
                }),
            ));
        }
        self.text_open = true;
        out
    }

    fn close_responses_message(&mut self) -> Vec<u8> {
        if !self.text_open {
            return Vec::new();
        }
        self.text_open = false;
        let text = self.text_buffer.clone();
        let mut out = Vec::new();
        if !text.is_empty() {
            out.extend(frame(
                "response.output_text.done",
                json!({
                    "type": "response.output_text.done",
                    "item_id": format!("msg_{}", self.id),
                    "output_index": 0,
                    "content_index": 0,
                    "text": text,
                }),
            ));
        }
        if self.message_open {
            self.message_open = false;
            out.extend(frame(
                "response.content_part.done",
                json!({
                    "type": "response.content_part.done",
                    "item_id": format!("msg_{}", self.id),
                    "output_index": 0,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": text, "annotations": []}
                }),
            ));
            out.extend(frame(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "id": format!("msg_{}", self.id),
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text, "annotations": []}],
                    }
                }),
            ));
        }
        out
    }

    fn close_block(&mut self) -> Vec<u8> {
        let Some((index, _)) = self.open_block.take() else {
            return Vec::new();
        };
        frame(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        )
    }

    fn stop(&mut self, reason: StopReason) -> Vec<u8> {
        // A turn that opened a tool call always reports tool_use, whatever the
        // upstream called it.
        let reason = if self.calls.values().any(|call| !call.name.is_empty()) {
            StopReason::ToolUse
        } else {
            reason
        };
        match self.protocol {
            WireProtocol::OpenaiChat => {
                let mut out =
                    self.chat_chunk(json!({}), json!(stop_reason_to(self.protocol, reason)));
                if self.usage.output_tokens > 0 {
                    out.extend(data_frame(json!({
                        "id": self.id,
                        "choices": [],
                        "usage": {
                            "prompt_tokens": self.usage.input_tokens,
                            "completion_tokens": self.usage.output_tokens,
                            "total_tokens":
                                self.usage.input_tokens + self.usage.output_tokens,
                        }
                    })));
                }
                out.extend_from_slice(b"data: [DONE]\n\n");
                out
            }
            WireProtocol::OpenaiResponses => {
                let mut out = self.close_responses_message();
                let mut output = Vec::new();
                if !self.text_buffer.is_empty() {
                    output.push(json!({
                        "type": "message",
                        "id": format!("msg_{}", self.id),
                        "status": "completed",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": self.text_buffer,
                            "annotations": [],
                        }],
                    }));
                }
                for (index, call) in &self.calls {
                    let item_id = self.call_item_id(*index);
                    out.extend(frame(
                        "response.output_item.done",
                        json!({
                            "type": "response.output_item.done",
                            "output_index": 0,
                            "item": {
                                "id": item_id,
                                "type": "function_call",
                                "call_id": call.id,
                                "name": call.name,
                                "arguments": call.arguments,
                                "status": "completed",
                            }
                        }),
                    ));
                    output.push(json!({
                        "type": "function_call",
                        "id": item_id,
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                        "status": "completed",
                    }));
                }
                let status = if reason == StopReason::MaxTokens {
                    "incomplete"
                } else {
                    "completed"
                };
                out.extend(frame(
                    "response.completed",
                    json!({
                        "type": "response.completed",
                        "response": self.envelope(status, &output),
                    }),
                ));
                out
            }
            WireProtocol::AnthropicMessages => {
                let mut out = self.close_block();
                out.extend(frame(
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": {
                            "stop_reason": stop_reason_to(self.protocol, reason),
                            "stop_sequence": Value::Null,
                        },
                        "usage": {"output_tokens": self.usage.output_tokens},
                    }),
                ));
                out.extend(frame("message_stop", json!({"type": "message_stop"})));
                out
            }
            WireProtocol::GeminiGenerateContent => {
                // Only the tool calls are flushed here. Text was already sent
                // as deltas, and Gemini's final frame repeats the candidate
                // block, so re-emitting it would double the visible text.
                let mut parts = Vec::new();
                for call in self.calls.values() {
                    let args = serde_json::from_str::<Value>(&call.arguments)
                        .unwrap_or_else(|_| json!({}));
                    parts.push(json!({"functionCall": {"name": call.name, "args": args}}));
                }
                data_frame(json!({
                    "candidates": [{
                        "content": {"role": "model", "parts": parts},
                        "finishReason": stop_reason_to(self.protocol, reason),
                        "index": 0,
                    }],
                    "usageMetadata": {
                        "promptTokenCount": self.usage.input_tokens,
                        "candidatesTokenCount": self.usage.output_tokens,
                        "totalTokenCount":
                            self.usage.input_tokens + self.usage.output_tokens,
                    },
                    "modelVersion": self.model,
                    "responseId": self.id,
                }))
            }
        }
    }
}

/// An SSE frame carrying both `event:` and `data:` lines.
fn frame(event: &str, data: Value) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

/// An SSE frame carrying only a `data:` line (OpenAI and Gemini style).
fn data_frame(data: Value) -> Vec<u8> {
    format!("data: {data}\n\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(protocol: WireProtocol, upstream: &str) -> Vec<StreamEvent> {
        let mut decoder = StreamDecoder::new(protocol);
        let mut events = decoder.push(upstream.as_bytes());
        events.extend(decoder.finish());
        events
    }

    fn render(protocol: WireProtocol, events: &[StreamEvent]) -> String {
        let mut encoder = StreamEncoder::new(protocol, "client-model");
        let mut out = String::from_utf8(encoder.start()).expect("valid utf8 preamble");
        for event in events {
            out.push_str(&String::from_utf8(encoder.push(event)).expect("valid utf8 frame"));
        }
        out.push_str(&String::from_utf8(encoder.finish()).expect("valid utf8 trailer"));
        out
    }

    fn text_of(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    const CHAT_STREAM: &str = concat!(
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );

    const ANTHROPIC_STREAM: &str = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );

    const RESPONSES_STREAM: &str = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n\n"
    );

    const GEMINI_STREAM: &str = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hel\"}]},\"index\":0}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"lo\"}]},\"index\":0}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":2,\"candidatesTokenCount\":6}}\n\n"
    );

    const ALL: [WireProtocol; 4] = [
        WireProtocol::OpenaiChat,
        WireProtocol::OpenaiResponses,
        WireProtocol::AnthropicMessages,
        WireProtocol::GeminiGenerateContent,
    ];

    fn sample_streams() -> [(WireProtocol, &'static str); 4] {
        [
            (WireProtocol::OpenaiChat, CHAT_STREAM),
            (WireProtocol::OpenaiResponses, RESPONSES_STREAM),
            (WireProtocol::AnthropicMessages, ANTHROPIC_STREAM),
            (WireProtocol::GeminiGenerateContent, GEMINI_STREAM),
        ]
    }

    #[test]
    fn every_upstream_decodes_to_the_same_text_and_a_stop() {
        for (protocol, stream) in sample_streams() {
            let events = collect(protocol, stream);
            assert_eq!(text_of(&events), "Hello", "{protocol} text");
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, StreamEvent::Stop { .. })),
                "{protocol} never signalled completion"
            );
        }
    }

    /// Re-decode SSE we rendered, returning the text a client would reassemble.
    fn text_seen_by_client(client: WireProtocol, sse: &str) -> String {
        text_of(&collect(client, sse))
    }

    #[test]
    fn text_crosses_every_upstream_to_client_pair() {
        // The property the whole feature exists for: any provider dialect can
        // be replayed to any client dialect. Asserted by re-decoding the SSE we
        // rendered rather than by substring matching, because streamed text is
        // split across frames and only the client parser can reassemble it.
        for (upstream, stream) in sample_streams() {
            let events = collect(upstream, stream);
            for client in ALL {
                let rendered = render(client, &events);
                assert_eq!(
                    text_seen_by_client(client, &rendered),
                    "Hello",
                    "{upstream} -> {client} lost the text"
                );
            }
        }
    }

    #[test]
    fn tool_call_crosses_every_upstream_to_client_pair() {
        let streams = [
            (
                WireProtocol::OpenaiChat,
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"x\\\"}\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n"
                ),
            ),
            (
                WireProtocol::OpenaiResponses,
                concat!(
                    "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_0\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"lookup\"}}\n\n",
                    "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_0\",\"delta\":\"{\\\"q\\\":\\\"x\\\"}\"}\n\n",
                    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{}}}\n\n"
                ),
            ),
            (
                WireProtocol::AnthropicMessages,
                concat!(
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"lookup\"}}\n\n",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\\\"x\\\"}\"}}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n"
                ),
            ),
            (
                WireProtocol::GeminiGenerateContent,
                "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"lookup\",\"args\":{\"q\":\"x\"}}}]},\"finishReason\":\"STOP\"}]}\n\n",
            ),
        ];

        for (upstream, stream) in streams {
            let events = collect(upstream, stream);
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    StreamEvent::ToolCallStart { name, .. } if name == "lookup"
                )),
                "{upstream} did not decode the tool call"
            );
            for client in ALL {
                let rendered = render(client, &events);
                let round_tripped = collect(client, &rendered);
                let name = round_tripped
                    .iter()
                    .find_map(|event| match event {
                        StreamEvent::ToolCallStart { name, .. } => Some(name.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        panic!("{upstream} -> {client} lost the tool call entirely")
                    });
                assert_eq!(name, "lookup", "{upstream} -> {client} changed the name");
                // The arguments must reassemble into the exact object that went
                // in: a partial or duplicated payload fails to parse.
                let arguments: String = round_tripped
                    .iter()
                    .filter_map(|event| match event {
                        StreamEvent::ToolCallDelta { arguments, .. } => Some(arguments.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    serde_json::from_str::<Value>(&arguments).ok(),
                    Some(json!({"q": "x"})),
                    "{upstream} -> {client} produced {arguments}"
                );
            }
        }
    }

    #[test]
    fn tool_arguments_are_not_duplicated_across_the_boundary() {
        // A decoder that replayed its accumulated buffer on finish would hand
        // the client `{"q":"x"}{"q":"x"}`.
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n"
        );
        let events = collect(WireProtocol::OpenaiChat, stream);
        let joined: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ToolCallDelta { arguments, .. } => Some(arguments.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(joined, r#"{"a":1}"#);
    }

    #[test]
    fn anthropic_never_leaves_two_blocks_open() {
        let events = vec![
            StreamEvent::TextDelta {
                text: "hi".to_string(),
            },
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call_1".to_string(),
                name: "lookup".to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                arguments: "{}".to_string(),
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
            },
        ];
        let rendered = render(WireProtocol::AnthropicMessages, &events);
        let starts = rendered.matches("\"type\":\"content_block_start\"").count();
        let stops = rendered.matches("\"type\":\"content_block_stop\"").count();
        assert_eq!(starts, 2, "expected one text block and one tool block");
        assert_eq!(stops, 2, "every opened block must be closed");
        assert!(rendered.contains("message_stop"));
    }

    #[test]
    fn anthropic_block_indexes_are_sequential() {
        let events = vec![
            StreamEvent::TextDelta {
                text: "a".to_string(),
            },
            StreamEvent::ToolCallStart {
                index: 0,
                id: "c".to_string(),
                name: "f".to_string(),
            },
            StreamEvent::ThinkingDelta {
                text: "b".to_string(),
            },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        let rendered = render(WireProtocol::AnthropicMessages, &events);
        // Parse the frames: serde_json orders object keys, so matching an
        // adjacent `"type":..,"index":..` literal would be unreliable.
        let mut opened = Vec::new();
        let mut closed = Vec::new();
        for line in rendered.lines() {
            let Some(payload) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            match value["type"].as_str() {
                Some("content_block_start") => opened.push(value["index"].as_u64()),
                Some("content_block_stop") => closed.push(value["index"].as_u64()),
                _ => {}
            }
        }
        assert_eq!(opened, vec![Some(0), Some(1), Some(2)]);
        assert_eq!(closed, vec![Some(0), Some(1), Some(2)]);
    }

    #[test]
    fn chat_stream_ends_with_the_done_sentinel() {
        let events = collect(WireProtocol::OpenaiChat, CHAT_STREAM);
        let rendered = render(WireProtocol::OpenaiChat, &events);
        assert!(rendered.trim_end().ends_with("data: [DONE]"));
    }

    #[test]
    fn byte_at_a_time_feeding_loses_nothing() {
        // SSE frames arrive split at arbitrary boundaries; a correct decoder
        // buffers partial lines and produces the identical event sequence.
        let mut decoder = StreamDecoder::new(WireProtocol::OpenaiChat);
        let mut events = Vec::new();
        for byte in CHAT_STREAM.as_bytes() {
            events.extend(decoder.push(&[*byte]));
        }
        events.extend(decoder.finish());
        assert_eq!(text_of(&events), "Hello");
    }

    #[test]
    fn usage_crosses_the_boundary_into_openai_chat() {
        let events = collect(WireProtocol::AnthropicMessages, ANTHROPIC_STREAM);
        let rendered = render(WireProtocol::OpenaiChat, &events);
        assert!(
            rendered.contains("\"completion_tokens\":5"),
            "token counts must reach Chat clients: {rendered}"
        );
    }

    #[test]
    fn finish_is_idempotent() {
        let mut encoder = StreamEncoder::new(WireProtocol::OpenaiResponses, "m");
        let _ = encoder.start();
        let first = encoder.push(&StreamEvent::Stop {
            reason: StopReason::EndTurn,
        });
        let second = encoder.finish();
        assert!(!first.is_empty());
        assert!(
            second.is_empty(),
            "finish must not repeat the terminal frame"
        );
    }

    #[test]
    fn a_tool_call_forces_the_tool_use_stop_reason() {
        let events = vec![
            StreamEvent::ToolCallStart {
                index: 0,
                id: "c".to_string(),
                name: "f".to_string(),
            },
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        for client in ALL {
            let rendered = render(client, &events);
            let expected = stop_reason_to(client, StopReason::ToolUse);
            assert!(
                rendered.contains(expected),
                "{client} should report {expected}: {rendered}"
            );
        }
    }
}
