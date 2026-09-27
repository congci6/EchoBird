//! Protocol bridge — lets a client speak one wire protocol while the provider
//! speaks another.
//!
//! This is what makes protocol choice a free choice. A CLI such as Codex only
//! ever speaks the Responses API, but the model behind it may serve nothing but
//! Chat Completions. Pointing that CLI at this bridge lets EchoBird accept
//! `/v1/responses`, convert to whatever the provider actually implements, and
//! convert the answer back — so the user picks the protocol they want and the
//! provider's limits stop being the constraint.
//!
//! The bridge serves all four dialects on one port. Which upstream protocol a
//! given request is translated into is decided by the currently configured
//! [`BridgeTarget`], not by the URL the client called, so a tool can be
//! repointed at a different provider without reconfiguring it.

mod server;

pub use server::{build_router, BridgeState};

use crate::services::protocol::WireProtocol;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

/// Default loopback port. The sibling proxies use 53682/53683.
pub const PROTOCOL_BRIDGE_PORT: u16 = 53684;

const PORT_NAME: &str = "protocol-bridge";

/// Guards the one-time bind described in [spawn_proxy_task].
static STARTED: AtomicBool = AtomicBool::new(false);

/// The upstream a bridged request is forwarded to.
#[derive(Clone, Debug)]
pub struct BridgeTarget {
    /// Base URL for the protocol in `protocol`, without the endpoint path.
    pub base_url: String,
    pub api_key: String,
    /// Model id sent upstream. The bridge serves one model at a time, which is
    /// what a CLI expects when it is handed a base URL.
    pub model: String,
    /// The protocol the provider natively speaks.
    pub protocol: WireProtocol,
}

static TARGET: OnceLock<RwLock<Option<BridgeTarget>>> = OnceLock::new();

fn target_cell() -> &'static RwLock<Option<BridgeTarget>> {
    TARGET.get_or_init(|| RwLock::new(None))
}

/// Point the bridge at an upstream. Takes effect on the next request, so
/// switching models never restarts the listener or disturbs in-flight calls.
pub fn set_target(target: BridgeTarget) {
    match target_cell().write() {
        Ok(mut guard) => *guard = Some(target),
        // A poisoned lock means another thread panicked mid-update; the target
        // is plain data and safe to keep using.
        Err(poisoned) => *poisoned.into_inner() = Some(target),
    }
}

/// Stop forwarding. Requests already in flight complete against their captured
/// target; new ones fail fast rather than reaching a stale provider.
pub fn clear_target() {
    match target_cell().write() {
        Ok(mut guard) => *guard = None,
        Err(poisoned) => *poisoned.into_inner() = None,
    }
}

/// A snapshot of the current upstream, cloned so a request keeps serving
/// against the same provider even if the user switches models mid-stream.
pub fn target() -> Option<BridgeTarget> {
    match target_cell().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// The loopback port the bridge listens on.
pub fn port() -> u16 {
    crate::services::local_proxy::saved_port(PORT_NAME, PROTOCOL_BRIDGE_PORT)
        .unwrap_or(PROTOCOL_BRIDGE_PORT)
}

/// The base URL to hand a client that should speak through the bridge.
pub fn base_url() -> String {
    format!("http://127.0.0.1:{}", port())
}

/// Whether the bridge has somewhere to send traffic.
pub fn is_configured() -> bool {
    target().is_some()
}

// ─── Lifecycle ───

/// Bind and serve the bridge, mirroring the sibling proxies' startup so the
/// port is published before any tool config can reference it.
pub fn spawn_proxy_task() {
    // The bridge is a singleton: rebinding an already-bound port would make
    // `local_proxy::bind` fall back to a random one, so a second apply would
    // silently publish a different address than the one tools were given.
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Err(error) = bind_and_serve() {
        STARTED.store(false, Ordering::SeqCst);
        log::error!("[ProtocolBridge] {error}");
    }
}

fn bind_and_serve() -> Result<(), String> {
    let listener = crate::services::local_proxy::bind(PORT_NAME, PROTOCOL_BRIDGE_PORT)?;
    let app = build_router()?;
    let bound_port = listener.local_addr().map_err(|e| e.to_string())?.port();
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let listener = tokio::net::TcpListener::from_std(listener).map_err(|e| e.to_string())?;
    log::info!("[ProtocolBridge] listening on 127.0.0.1:{bound_port}");
    tauri::async_runtime::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            log::error!("[ProtocolBridge] serve failed: {error}");
        }
    });
    Ok(())
}

/// Point the bridge at an upstream and make sure it is listening.
///
/// Called on every model application: switching providers only swaps the
/// target, so an already-running listener (and any in-flight request) is left
/// alone.
pub fn ensure_serving(target: BridgeTarget) {
    set_target(target);
    spawn_proxy_task();
}
#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BridgeTarget {
        BridgeTarget {
            base_url: "https://provider.example/v1".to_string(),
            api_key: "k".to_string(),
            model: "m".to_string(),
            protocol: WireProtocol::OpenaiChat,
        }
    }

    #[test]
    fn base_url_targets_loopback_on_the_configured_port() {
        let url = base_url();
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert!(url.ends_with(&port().to_string()), "{url}");
    }

    #[test]
    fn set_and_clear_target_round_trip() {
        set_target(sample());
        let current = target().expect("target is set");
        assert_eq!(current.base_url, "https://provider.example/v1");
        assert_eq!(current.protocol, WireProtocol::OpenaiChat);
        assert!(is_configured());
        clear_target();
        assert!(target().is_none());
        assert!(!is_configured());
    }
}
