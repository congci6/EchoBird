//! The DeepSeek Harness desktop app (`dsh`) was removed from EchoBird.
//!
//! This is a removal, not a behaviour change, so the test that matters is the
//! negative one: nothing in the shipped tool surface may reintroduce the id.
//!
//! A removal test is worth writing rather than just deleting the files, because
//! the tool list is assembled from two independent sources that are easy to
//! half-update:
//!
//! - `bundled_assets.rs` names the ids registered at compile time, and
//! - `docs/api/tools/install/index.json` is the *remote* registry, which
//!   AGENTS.md records as winning over the bundled copy at runtime.
//!
//! Deleting `tools/dsh/` and the Rust module while leaving the id in either
//! list yields a tool the app advertises but cannot configure — an entry that
//! appears in the picker and then fails or does nothing when applied.

use echobird_lib::services::bundled_assets::INSTALLABLE_TOOL_IDS;

/// The removed id, spelled out once so a future edit cannot quietly move it.
const REMOVED: &str = "dsh";

/// The remote registry must not still advertise the tool.
///
/// This is the list that actually decides the tool surface: `docs/api/**` is
/// fetched and preferred over the bundled copy, so an id left here survives a
/// complete removal of the Rust and frontend code.
#[test]
fn remote_registry_no_longer_lists_the_removed_tool() {
    let index: serde_json::Value =
        serde_json::from_str(include_str!("../../docs/api/tools/install/index.json"))
            .expect("remote tool registry is valid JSON");

    let ids = index
        .get("ids")
        .and_then(serde_json::Value::as_array)
        .expect("registry exposes an `ids` array");

    assert!(
        !ids.iter().any(|id| id.as_str() == Some(REMOVED)),
        "remote registry still advertises `{REMOVED}`: {ids:?}"
    );
}

/// The compile-time tool list must not carry the removed id.
///
/// This list is baked into the binary, so it decides what ships even with no
/// network — and it also feeds the agent's system prompt, which is why an id
/// left behind here would keep the tool documented after its code is gone.
#[test]
fn bundled_assets_exclude_the_removed_tool() {
    assert!(
        !INSTALLABLE_TOOL_IDS.contains(&REMOVED),
        "bundled installable-tool ids still include `{REMOVED}`"
    );
}
