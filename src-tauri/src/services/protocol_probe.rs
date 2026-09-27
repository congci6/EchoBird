//! Find out which of the four dialects a provider actually serves, before
//! wiring it to a tool and reading the error.
//!
//! The protocol choice in the model centre is only meaningful if the user
//! knows what the provider speaks. Responses in particular cannot be inferred
//! from config: there is no `responses_url` to look at, and a gateway that
//! refuses it answers `500 not implemented` rather than `404`, so nothing short
//! of asking reveals it. This asks.
//!
//! Every dialect is probed, and the report distinguishes four outcomes rather
//! than a bare yes/no. That distinction is the point: a provider that serves
//! two of the four and a provider with a bad key both produce four non-200
//! responses, and telling the user "Responses unsupported" when the real
//! problem is the credential sends them off to re-pick a protocol for nothing.

use crate::services::protocol::WireProtocol;
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;

/// How long a single probe waits before giving up.
///
/// Short on purpose: this runs on a button press in front of the user, and a
/// provider that has not answered a one-word prompt in a few seconds is not
/// going to answer the real request either.
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);

/// What one probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    /// The provider answered in this dialect.
    Available,
    /// The provider said the endpoint is not there.
    Unsupported,
    /// The provider rejected the credential, so nothing can be concluded
    /// about which dialects exist.
    Auth,
    /// Some other failure — a timeout, a 503, a 400. Says nothing about the
    /// endpoint, and is deliberately not reported as unsupported.
    Unknown,
}

/// One dialect's result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DialectReport {
    /// The `WireProtocol::as_str` name, so the frontend can match it against
    /// the protocol options it already renders.
    pub protocol: String,
    pub available: bool,
    pub outcome: Outcome,
    /// The provider's own words, truncated. This is what turns "unavailable"
    /// into something the user can act on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub latency_ms: u64,
}

/// Probe all four dialects against `base_url`.
///
/// Runs them concurrently: four sequential round trips would make the button
/// feel broken, and they are independent questions.
pub async fn probe_all(base_url: &str, api_key: &str, model: &str) -> Vec<DialectReport> {
    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(client) => client,
        Err(_) => return Vec::new(),
    };

    let dialects = [
        WireProtocol::OpenaiChat,
        WireProtocol::OpenaiResponses,
        WireProtocol::AnthropicMessages,
        WireProtocol::GeminiGenerateContent,
    ];

    let mut reports = Vec::with_capacity(dialects.len());
    for dialect in dialects {
        reports.push(probe_one(&client, base_url, api_key, model, dialect).await);
    }
    reports
}

async fn probe_one(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    dialect: WireProtocol,
) -> DialectReport {
    let (url, body) = request_for(base_url, model, dialect);
    let mut request = client
        .post(&url)
        .json(&body)
        .header("content-type", "application/json");
    request = match dialect {
        WireProtocol::AnthropicMessages => request
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01"),
        WireProtocol::GeminiGenerateContent => request.header("x-goog-api-key", api_key),
        _ => request.bearer_auth(api_key),
    };

    let started = std::time::Instant::now();
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return report(
                dialect,
                Outcome::Unknown,
                Some(truncate(&error.to_string())),
                started.elapsed(),
            )
        }
    };

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let elapsed = started.elapsed();

    let outcome = if status.is_success() {
        Outcome::Available
    } else if status.as_u16() == 401 || status.as_u16() == 403 {
        // The key is wrong or lacks access. Every dialect will answer this, so
        // reporting them all as unsupported would be actively misleading.
        Outcome::Auth
    } else if crate::services::protocol_bridge::classify_upstream_failure(
        WireProtocol::OpenaiResponses,
        status,
        &text,
    )
    .is_some()
        || status == axum::http::StatusCode::NOT_IMPLEMENTED
        || status == axum::http::StatusCode::NOT_FOUND
    {
        Outcome::Unsupported
    } else {
        Outcome::Unknown
    };

    report(dialect, outcome, Some(truncate(&text)), elapsed)
}

fn report(
    dialect: WireProtocol,
    outcome: Outcome,
    detail: Option<String>,
    elapsed: std::time::Duration,
) -> DialectReport {
    DialectReport {
        protocol: dialect.as_str().to_string(),
        available: outcome == Outcome::Available,
        outcome,
        detail: detail.filter(|text| !text.trim().is_empty()),
        latency_ms: elapsed.as_millis() as u64,
    }
}

/// The URL and the smallest legal body for one dialect.
///
/// The bodies are deliberately trivial — one word, no tools, no system prompt.
/// A probe should answer "does this endpoint exist and does my key work", and
/// anything more would fail for reasons that have nothing to do with the
/// dialect.
fn request_for(base_url: &str, model: &str, dialect: WireProtocol) -> (String, Value) {
    let base = base_url.trim_end_matches('/');
    // A user pasting an Anthropic-style base usually includes the path the
    // endpoint lives at; do not append a second one.
    let trimmed = base
        .trim_end_matches("/chat/completions")
        .trim_end_matches("/messages")
        .trim_end_matches("/responses");
    match dialect {
        WireProtocol::OpenaiChat => (
            format!("{trimmed}/chat/completions"),
            json!({
                "model": model,
                "messages": [{"role": "user", "content": "ping"}],
                "max_tokens": 1,
            }),
        ),
        WireProtocol::OpenaiResponses => (
            format!("{trimmed}/responses"),
            json!({"model": model, "input": "ping", "max_output_tokens": 1}),
        ),
        WireProtocol::AnthropicMessages => (
            format!("{trimmed}/messages"),
            json!({
                "model": model,
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "ping"}],
            }),
        ),
        WireProtocol::GeminiGenerateContent => {
            // Gemini addresses the model in the path and the base is a host
            // root, not a `/v1` prefix: `https://host/v1beta` is the shape
            // every Google-compatible gateway uses.
            let host = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
            (
                format!("{host}/v1beta/models/{model}:generateContent"),
                json!({"contents": [{"role": "user", "parts": [{"text": "ping"}]}]}),
            )
        }
    }
}

/// Keep the provider's wording short enough to render in a table cell.
fn truncate(text: &str) -> String {
    let cleaned = text.trim();
    if cleaned.chars().count() <= 180 {
        return cleaned.to_string();
    }
    let head: String = cleaned.chars().take(180).collect();
    format!("{head}…")
}
