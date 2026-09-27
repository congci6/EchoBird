//! Protocol-neutral request/response model shared by every wire protocol.
//!
//! EchoBird lets a user pick which wire protocol a model is used through, even
//! when the provider only speaks a different one. Every conversion therefore
//! goes through the types in this module: an inbound request in protocol A is
//! parsed into [`CanonicalRequest`], re-encoded as protocol B for the upstream,
//! and the upstream's answer (streaming or not) is normalized back through
//! [`CanonicalResponse`] / [`StreamEvent`] before being re-encoded in the
//! protocol the caller actually speaks.
//!
//! Keeping a single IR is what makes arbitrary pairwise conversion work: with
//! N protocols only N parsers and N encoders are needed instead of N*(N-1)
//! direct translators.

use serde::{Deserialize, Serialize};
mod encode;
mod request;
mod response;
mod stream;

pub use encode::{encode_request, stop_reason_from, stop_reason_to};
pub use request::parse_request;
pub use response::{encode_response, parse_response};
pub use stream::{StreamDecoder, StreamEncoder};

use serde_json::Value;

/// Current Unix time in seconds. OpenAI-shaped responses carry a `created`
/// timestamp; using the wall clock keeps that field meaningful after a
/// cross-protocol conversion.
pub fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
/// The wire protocols EchoBird can speak on a model's behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireProtocol {
    /// OpenAI `POST /v1/chat/completions`.
    OpenaiChat,
    /// OpenAI `POST /v1/responses`.
    OpenaiResponses,
    /// Anthropic `POST /v1/messages`.
    AnthropicMessages,
    /// Google `POST /v1beta/models/{model}:generateContent`.
    GeminiGenerateContent,
}

impl WireProtocol {
    /// Stable identifier used in config files and IPC payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiChat => "openai-chat",
            Self::OpenaiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GeminiGenerateContent => "gemini-generate-content",
        }
    }

    /// Parse an identifier, accepting the legacy short spellings the rest of
    /// the app already persisted (`openai` / `anthropic`) so configs written
    /// before protocol selection existed keep working.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "openai-chat" | "chat-completions" | "chat" | "openai" => Some(Self::OpenaiChat),
            "openai-responses" | "responses" => Some(Self::OpenaiResponses),
            "anthropic-messages" | "anthropic" | "messages" => Some(Self::AnthropicMessages),
            "gemini-generate-content" | "gemini" | "generate-content" | "generatecontent" => {
                Some(Self::GeminiGenerateContent)
            }
            _ => None,
        }
    }

    /// Whether the protocol addresses a specific model inside the path. Gemini
    /// embeds the model id in the URL rather than the JSON body, so callers
    /// building a request must pull it back out of the path.
    pub fn model_in_path(self) -> bool {
        matches!(self, Self::GeminiGenerateContent)
    }

    /// The auth header style the protocol expects. Gemini uses a `x-goog-api-key`
    /// query parameter or header, OpenAI and Anthropic use bearer / `x-api-key`.
    pub fn auth_style(self) -> AuthStyle {
        match self {
            Self::AnthropicMessages => AuthStyle::AnthropicKey,
            Self::GeminiGenerateContent => AuthStyle::GoogleKey,
            Self::OpenaiChat | Self::OpenaiResponses => AuthStyle::Bearer,
        }
    }
}

impl std::fmt::Display for WireProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a provider expects its credential to be presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// `x-api-key: <key>` plus `anthropic-version`.
    AnthropicKey,
    /// `x-goog-api-key: <key>`.
    GoogleKey,
}

/// Who authored a canonical message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A single piece of message content, normalized across all four protocols.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// Base64 image data. `data` excludes the `data:` URI prefix; encoders add
    /// whatever prefix or inline shape their protocol expects.
    Image {
        media_type: String,
        data: String,
    },
    /// An assistant request to invoke a tool.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// The caller's answer to a previous [`ContentBlock::ToolUse`].
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// Extended-thinking / reasoning output. `signature` is Anthropic's opaque
    /// verification blob; other protocols drop it on the way out.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

impl ContentBlock {
    /// Convenience constructor for the overwhelmingly common text case.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// One turn in the conversation. Tool results are carried on the user turn
/// that produced them, matching Anthropic's model; other protocols re-home
/// them during encoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl CanonicalMessage {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON Schema describing the tool's parameters.
    pub parameters: Value,
}

/// A provider's extended-thinking configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingConfig {
    /// Token budget for the thinking phase.
    pub budget_tokens: u32,
}

/// A fully normalized inference request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CanonicalRequest {
    pub model: String,
    /// System / developer instructions, kept separate because Anthropic and
    /// Gemini model them as a distinct field while OpenAI folds them into the
    /// message list.
    #[serde(default)]
    pub system: Vec<ContentBlock>,
    pub messages: Vec<CanonicalMessage>,
    #[serde(default)]
    pub tools: Vec<CanonicalTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

impl CanonicalRequest {
    /// Whether any message carries tool results — used to skip fields a given
    /// upstream rejects when no tools are in play.
    pub fn has_tool_results(&self) -> bool {
        self.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        })
    }
}

/// Why generation halted, normalized across protocols.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    /// The upstream sent a reason we have no better mapping for; the original
    /// string is preserved so nothing is silently lost.
    Other,
}

/// Token accounting, normalized.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A fully normalized (non-streaming) response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CanonicalResponse {
    pub id: String,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

impl CanonicalResponse {
    /// Concatenate every text block, used for logging and for protocols that
    /// can only present a flat string.
    pub fn joined_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// One normalized step of a streaming response.
///
/// The bridge converts an upstream SSE stream into a sequence of these, then
/// re-encodes that sequence into the caller's protocol. Stateful encoders
/// (Anthropic's indexed content blocks, Responses' output items) live in
/// `stream.rs` and consume this event stream.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Assistant text arriving incrementally.
    TextDelta { text: String },
    /// Extended-thinking text arriving incrementally.
    ThinkingDelta { text: String },
    /// A tool call was opened. `index` is the upstream's own call index and is
    /// used to correlate subsequent [`StreamEvent::ToolCallDelta`] events.
    ToolCallStart {
        index: u32,
        id: String,
        name: String,
    },
    /// More JSON for the tool call at `index`.
    ToolCallDelta { index: u32, arguments: String },
    /// Final token counts, when the upstream reports them.
    Usage { usage: Usage },
    /// Terminal event. Always the last event of a stream.
    Stop { reason: StopReason },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_identifiers_round_trip() {
        for protocol in [
            WireProtocol::OpenaiChat,
            WireProtocol::OpenaiResponses,
            WireProtocol::AnthropicMessages,
            WireProtocol::GeminiGenerateContent,
        ] {
            assert_eq!(WireProtocol::parse(protocol.as_str()), Some(protocol));
        }
    }

    #[test]
    fn parse_accepts_legacy_and_spaced_spellings() {
        assert_eq!(
            WireProtocol::parse("OpenAI"),
            Some(WireProtocol::OpenaiChat)
        );
        assert_eq!(
            WireProtocol::parse(" anthropic "),
            Some(WireProtocol::AnthropicMessages)
        );
        assert_eq!(
            WireProtocol::parse("openai_responses"),
            Some(WireProtocol::OpenaiResponses)
        );
        assert_eq!(
            WireProtocol::parse("generateContent"),
            Some(WireProtocol::GeminiGenerateContent)
        );
        assert_eq!(WireProtocol::parse("grpc"), None);
    }

    #[test]
    fn only_gemini_addresses_the_model_in_the_path() {
        assert!(WireProtocol::GeminiGenerateContent.model_in_path());
        assert!(!WireProtocol::OpenaiChat.model_in_path());
        assert!(!WireProtocol::OpenaiResponses.model_in_path());
        assert!(!WireProtocol::AnthropicMessages.model_in_path());
    }

    #[test]
    fn joined_text_ignores_non_text_blocks() {
        let response = CanonicalResponse {
            content: vec![
                ContentBlock::text("hello "),
                ContentBlock::Thinking {
                    text: "ignored".to_string(),
                    signature: None,
                },
                ContentBlock::text("world"),
            ],
            ..Default::default()
        };
        assert_eq!(response.joined_text(), "hello world");
    }

    #[test]
    fn has_tool_results_scans_every_turn() {
        let request = CanonicalRequest {
            messages: vec![
                CanonicalMessage::user_text("hi"),
                CanonicalMessage {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: "42".to_string(),
                        is_error: false,
                    }],
                },
            ],
            ..Default::default()
        };
        assert!(request.has_tool_results());
        assert!(!CanonicalRequest::default().has_tool_results());
    }
}
