//! Applying a model to Codex must not depend on the shape of the user's
//! profile directory.
//!
//! Windows users routinely have `%USERPROFILE%` pointing at a junction — a
//! OneDrive-backed profile, a relocated `C:\Users`, an `mklink /J` home. Any
//! filesystem call that *walks* the path (`Path::exists`, `create_dir_all`)
//! then fails with
//!
//! ```text
//! 无法遍历该路径，因为它包含不受信任的装入点。 (os error 448)
//! ```
//!
//! — `ERROR_UNTRUSTED_MOUNT_POINT`, raised when a reparse point in the chain
//! is not on the trusted-mount list. Writing a file with a plain `fs::write`
//! and letting the OS create the leaf does not walk anything, so it is not
//! affected; the pre-flight `exists()` checks are.
//!
//! `~/.codex` is the Codex CLI's own directory, not EchoBird's, and the CLI
//! creates it itself when it is missing, so "is my home a junction" is not
//! something an apply should be able to fail on.

use echobird_lib::models::model::ModelConfig;
use echobird_lib::services::tool_config_manager::ModelInfo;

fn model_info() -> ModelInfo {
    serde_json::from_value(serde_json::json!({
        "name": "test",
        "model": "m",
        "baseUrl": "https://provider.example/v1",
        "apiKey": "k",
    }))
    .unwrap()
}

/// `apply_codex_at` is `pub(crate)`, so the crate's own tests reach it while
/// integration tests go through the public `apply_model_to_tool`. This test
/// pins the behaviour that matters: a codex home that cannot be probed by
/// `exists()` must not stop the apply.
#[tokio::test]
async fn an_unprobeable_codex_home_does_not_fail_the_apply() {
    let dir = std::env::temp_dir().join("echobird-codex-home-probe");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    // A path whose *parent* does not exist, standing in for a home the OS will
    // not let us walk. `create_dir_all` on this is the call that fails on a
    // junction; `fs::write` on the leaf is not.
    let codex_dir = dir.join("nested").join(".codex");

    let result = echobird_lib::services::tool_config_manager::codex::apply_codex_at_for_test(
        "codex",
        &model_info(),
        &codex_dir,
        &state,
    );

    assert!(
        result.success,
        "an unprobeable parent must not block the apply: {}",
        result.message
    );
    assert!(
        codex_dir.join("config.toml").exists(),
        "config.toml must still be written"
    );
    assert!(
        codex_dir.join("auth.json").exists(),
        "auth.json must still be written"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A readable existing config must be preserved in the ways apply already
/// promises: the file is rewritten wholesale, so the guarantee that matters is
/// that a second apply with identical input does not touch it (the no-op
/// short-circuit) and still reports success.
#[tokio::test]
async fn applying_twice_is_still_success_and_leaves_one_config() {
    let dir = std::env::temp_dir().join("echobird-codex-twice");
    let _ = std::fs::remove_dir_all(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let codex_dir = dir.join(".codex");

    let first = echobird_lib::services::tool_config_manager::codex::apply_codex_at_for_test(
        "codex",
        &model_info(),
        &codex_dir,
        &state,
    );
    assert!(first.success, "{}", first.message);

    let second = echobird_lib::services::tool_config_manager::codex::apply_codex_at_for_test(
        "codex",
        &model_info(),
        &codex_dir,
        &state,
    );
    assert!(second.success, "{}", second.message);

    assert_eq!(
        std::fs::read_dir(&codex_dir).unwrap().count(),
        2,
        "config.toml and auth.json, nothing else"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_model_config_round_trips_the_protocol_fields() {
    // Guards the struct fields this suite constructs, so a rename cannot leave
    // these tests silently building a different shape than production.
    let config: ModelConfig = serde_json::from_value(serde_json::json!({
        "internalId": "x",
        "name": "x",
        "baseUrl": "https://p.example/v1",
        "apiKey": "k",
        "modelId": "m",
        "apiProtocol": "openai-responses",
        "responsesFallback": true,
        "autoDegradeProtocols": true,
    }))
    .unwrap();
    let json = serde_json::to_value(&config).unwrap();
    assert_eq!(json["apiProtocol"], "openai-responses");
    assert_eq!(json["responsesFallback"], true);
    assert_eq!(json["autoDegradeProtocols"], true);
}
