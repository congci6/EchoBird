//! Deciding whether an upstream failure means "this provider does not serve
//! that dialect", as opposed to "something went wrong this time".
//!
//! The distinction matters because the answer is acted on by rewriting the
//! route: degrade and the user's request works, treat as transient and it
//! fails again. A gateway that answers `500 not implemented` to `/v1/responses`
//! is a fixed fact about that provider, while a `503` or a timeout says
//! nothing about which dialects it has. Retrying or degrading on the latter
//! would hide a real outage behind a silently different route.

use axum::http::StatusCode;
use echobird_lib::services::protocol::WireProtocol;
use echobird_lib::services::protocol_bridge::classify_upstream_failure;

fn body_with(fragment: &str) -> String {
    format!("{{\"error\":{{\"code\":500,\"message\":\"{fragment}\"}}}}")
}

/// A provider that answers `not implemented` is telling us the dialect does
/// not exist for it. That is the one signal that justifies degrading.
#[test]
fn not_implemented_is_a_verdict_about_the_dialect() {
    for fragment in [
        "not implemented",
        "not implemented (request id: abc)",
        "NOT IMPLEMENTED",
        "this endpoint is not implemented",
    ] {
        assert_eq!(
            classify_upstream_failure(
                WireProtocol::OpenaiResponses,
                StatusCode::INTERNAL_SERVER_ERROR,
                &body_with(fragment),
            ),
            Some(WireProtocol::OpenaiChat),
            "{fragment:?} should degrade to Chat"
        );
    }
}

/// `unsupported` / `not supported` are the same verdict in other words. A
/// provider that says so has told us the endpoint is not there, not that it
/// had a bad minute.
#[test]
fn unsupported_and_not_supported_also_degrade() {
    for fragment in [
        "unsupported protocol",
        "unsupported",
        "not supported",
        "this model does not support the responses api",
    ] {
        assert_eq!(
            classify_upstream_failure(
                WireProtocol::OpenaiResponses,
                StatusCode::BAD_GATEWAY,
                &body_with(fragment),
            ),
            Some(WireProtocol::OpenaiChat),
            "{fragment:?} should degrade to Chat"
        );
    }
}

/// A 501 is the HTTP-level way of saying the same thing, and needs no wording
/// to be recognised.
#[test]
fn not_implemented_status_degrades_even_with_an_unhelpful_body() {
    assert_eq!(
        classify_upstream_failure(
            WireProtocol::OpenaiResponses,
            StatusCode::NOT_IMPLEMENTED,
            "",
        ),
        Some(WireProtocol::OpenaiChat)
    );
}

/// The wording only counts on a server error. A 400 carrying "unsupported" is
/// the provider complaining about the request we built — which may well be our
/// bug — and rewriting the route would paper over it.
#[test]
fn a_client_error_never_degrades_however_it_is_worded() {
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
        StatusCode::TOO_MANY_REQUESTS,
    ] {
        assert_eq!(
            classify_upstream_failure(
                WireProtocol::OpenaiResponses,
                status,
                &body_with("not implemented"),
            ),
            None,
            "{status} must not trigger a degrade"
        );
    }
}

/// A plain 5xx is a fault, not a verdict. The provider may well serve
/// `/v1/responses` perfectly well on the next call, and swapping dialects
/// would hide the outage.
#[test]
fn a_bare_server_error_is_treated_as_transient() {
    for status in [
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::BAD_GATEWAY,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::GATEWAY_TIMEOUT,
    ] {
        assert_eq!(
            classify_upstream_failure(
                WireProtocol::OpenaiResponses,
                status,
                &body_with("internal error"),
            ),
            None,
            "{status} is transient, not a verdict"
        );
    }
}

/// Only Responses has a fallback. Every other dialect already resolves to
/// something the provider serves, or the failure is about the request rather
/// than the endpoint, so there is nothing to degrade to.
#[test]
fn no_other_dialect_degrades() {
    for protocol in [
        WireProtocol::OpenaiChat,
        WireProtocol::AnthropicMessages,
        WireProtocol::GeminiGenerateContent,
    ] {
        assert_eq!(
            classify_upstream_failure(
                protocol,
                StatusCode::INTERNAL_SERVER_ERROR,
                &body_with("not implemented"),
            ),
            None,
            "{protocol:?} has no fallback"
        );
    }
}

/// The wording is matched case-insensitively and anywhere in the body, because
/// gateways put it at different depths — sometimes in `message`, sometimes
/// only in a `type` field, sometimes in a wrapper string.
#[test]
fn the_wording_is_found_wherever_it_sits() {
    let buried =
        r#"{"error":{"type":"api_error","message":"upstream failure"},"detail":"NOT IMPLEMENTED"}"#;
    assert_eq!(
        classify_upstream_failure(
            WireProtocol::OpenaiResponses,
            StatusCode::INTERNAL_SERVER_ERROR,
            buried,
        ),
        Some(WireProtocol::OpenaiChat)
    );
}
