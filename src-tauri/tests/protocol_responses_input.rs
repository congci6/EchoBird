//! The `input` shorthands OpenAI Responses allows, and which of them the
//! request decoder actually reads.
//!
//! Found by running the real bridge against a live provider: a Responses
//! client sending `{"input": "hi"}` came back from the supplier with
//! `field messages is required`. The decoder only ever read `input` as an
//! array, so the bare-string shorthand — the form Codex and the OpenAI SDKs
//! emit for a single-turn prompt — produced an empty conversation and the
//! request went upstream with `messages: []`.
//!
//! The array-of-objects form is the documented one and has to keep behaving
//! exactly as it did; the two shorthands are additive.

use echobird_lib::services::protocol::{parse_request, ContentBlock, Role, WireProtocol};
use serde_json::json;

fn responses(body: serde_json::Value) -> echobird_lib::services::protocol::CanonicalRequest {
    parse_request(WireProtocol::OpenaiResponses, &body, None).expect("decodes")
}

/// A bare string is the common shorthand — Codex, the OpenAI SDKs and most
/// clients emit it for a single-turn prompt.
#[test]
fn responses_input_shorthand_becomes_a_user_message() {
    let request = responses(json!({"model": "m", "input": "hi"}));
    assert_eq!(request.messages.len(), 1, "the prompt must not be dropped");
    assert_eq!(request.messages[0].role, Role::User);
    assert_eq!(request.messages[0].content, vec![ContentBlock::text("hi")]);
}

/// An empty string is not a turn. Sending it upstream as a real message would
/// ask the supplier to answer a blank prompt.
#[test]
fn responses_empty_input_shorthand_produces_no_message() {
    let request = responses(json!({"model": "m", "input": ""}));
    assert!(
        request.messages.is_empty(),
        "a blank prompt is not a turn: {:?}",
        request.messages
    );
}

/// An array may mix bare strings with message objects. A string element is the
/// text of a user turn, not an object to read `role`/`content` from — which is
/// how an element lands in a canonical message with no content at all.
#[test]
fn responses_input_string_array_becomes_one_user_message_per_element() {
    let request = responses(json!({
        "model": "m",
        "input": ["first", {"role": "assistant", "content": "second"}]
    }));
    assert_eq!(request.messages.len(), 2, "both elements must survive");
    assert_eq!(request.messages[0].role, Role::User);
    assert_eq!(
        request.messages[0].content,
        vec![ContentBlock::text("first")]
    );
    assert_eq!(request.messages[1].role, Role::Assistant);
    assert_eq!(
        request.messages[1].content,
        vec![ContentBlock::text("second")]
    );
}

/// The array-of-objects form is the documented one and is unchanged: the
/// shorthands above are additive, not a replacement.
#[test]
fn responses_input_object_array_is_unchanged() {
    let request = responses(json!({
        "model": "m",
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"}
        ]
    }));
    assert_eq!(request.messages.len(), 2);
    assert_eq!(request.messages[0].content, vec![ContentBlock::text("hi")]);
    assert!(
        matches!(request.messages[1].content[0], ContentBlock::ToolUse { .. }),
        "a function_call item is still a tool use: {:?}",
        request.messages[1].content
    );
}

/// The shorthand carries the prompt, so `instructions` still has to be read as
/// the system turn alongside it.
#[test]
fn responses_shorthand_keeps_instructions_as_system() {
    let request = responses(json!({"model": "m", "input": "hi", "instructions": "be terse"}));
    assert_eq!(request.system, vec![ContentBlock::text("be terse")]);
    assert_eq!(request.messages.len(), 1);
}
