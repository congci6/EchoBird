//! One-time migration support for configurations written by the removed
//! Codex Responses-to-Chat proxy.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::services::protocol::WireProtocol;

const LEGACY_PROXY_URL_FRAGMENT: &str = "127.0.0.1:53682";
const CONFIG_FILENAME: &str = "config.toml";
const LEGACY_RELAY_FILENAME: &str = "codex.json";

pub fn default_codex_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("CODEX_HOME") {
        let path = path.trim().trim_matches('"').trim_matches('\'').trim();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    if let Ok(path) = std::env::var("ECHOBIRD_CODEX_CONFIG_DIR") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    dirs::home_dir().map(|home| home.join(".codex"))
}

fn legacy_relay_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ECHOBIRD_RELAY_DIR") {
        if !path.is_empty() {
            return Some(PathBuf::from(path).join(LEGACY_RELAY_FILENAME));
        }
    }
    dirs::home_dir().map(|home| home.join(".echobird").join(LEGACY_RELAY_FILENAME))
}

/// Convert the last proxy-backed config to a direct Responses connection.
/// Returns `true` only when a legacy config was rewritten.
pub fn migrate_legacy_proxy_config(codex_dir: &Path) -> io::Result<bool> {
    let relay_path = legacy_relay_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "legacy Codex relay path unavailable",
        )
    })?;
    migrate_legacy_proxy_config_from(codex_dir, &relay_path)
}

fn migrate_legacy_proxy_config_from(codex_dir: &Path, relay_path: &Path) -> io::Result<bool> {
    let config_path = codex_dir.join(CONFIG_FILENAME);
    match fs::read_to_string(&config_path) {
        Ok(content) if content.contains(LEGACY_PROXY_URL_FRAGMENT) => {}
        Ok(_) | Err(_) => return Ok(false),
    }

    let relay: serde_json::Value = serde_json::from_str(&fs::read_to_string(relay_path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    let base_url = relay
        .get("baseUrl")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "legacy baseUrl is missing"))?;
    let model = relay
        .get("actualModel")
        .or_else(|| relay.get("modelName"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "legacy model is missing"))?;
    // The deleted proxy chose between forwarding `/v1/responses` verbatim and
    // translating to Chat from this flag. Translate-to-Chat was the default,
    // which is what the no-choice protocol now means, so only the passthrough
    // side needs carrying over — otherwise a user whose gateway really does
    // speak Responses would be silently downgraded to the bridge.
    let api_protocol = relay
        .get("responsesPassthrough")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
        .then(|| WireProtocol::OpenaiResponses.as_str().to_string());
    let model_info = crate::services::tool_config_manager::ModelInfo {
        api_protocol,
        name: relay
            .get("modelName")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        model: Some(model.to_string()),
        base_url: Some(base_url.to_string()),
        api_key: relay
            .get("apiKey")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        anthropic_url: None,
        protocol: Some("openai".to_string()),
        display_model: None,
        relay_mode: None,
        one_m_context: None,
    };
    let state_dir = relay_path.parent().unwrap_or_else(|| Path::new(""));
    let result = crate::services::tool_config_manager::apply_codex_at(
        "codex",
        &model_info,
        codex_dir,
        state_dir,
    );
    if result.success {
        Ok(true)
    } else {
        Err(io::Error::other(result.message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_tmpdir() -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "echobird_codex_migration_{}_{}",
            std::process::id(),
            id
        ));
        fs::create_dir_all(&dir).expect("create temp directory");
        dir
    }

    #[test]
    fn direct_config_is_untouched() {
        let dir = unique_tmpdir();
        let relay = dir.join(LEGACY_RELAY_FILENAME);
        fs::write(
            dir.join(CONFIG_FILENAME),
            "model = \"glm-5.2\"\nbase_url = \"https://example.com/v1\"\n",
        )
        .unwrap();

        assert!(!migrate_legacy_proxy_config_from(&dir, &relay).unwrap());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn proxy_config_is_migrated_to_direct_responses() {
        let dir = unique_tmpdir();
        let relay = dir.join(LEGACY_RELAY_FILENAME);
        fs::write(
            dir.join(CONFIG_FILENAME),
            "model_provider = \"OpenAI\"\nmodel = \"gpt-5.5\"\n\n[model_providers.OpenAI]\nbase_url = \"http://127.0.0.1:53682/v1\"\n",
        )
        .unwrap();
        fs::write(
            &relay,
            serde_json::json!({
                "apiKey": "test-key",
                "baseUrl": "https://provider.example/v1",
                "actualModel": "provider-model",
                "modelName": "Provider Model"
            })
            .to_string(),
        )
        .unwrap();

        assert!(migrate_legacy_proxy_config_from(&dir, &relay).unwrap());
        let config = fs::read_to_string(dir.join(CONFIG_FILENAME)).unwrap();
        assert!(config.contains("model = \"provider-model\""));
        assert!(config.contains("base_url = \"https://provider.example/v1\""));
        assert!(config.contains("wire_api = \"responses\""));
        assert!(config.contains("web_search = \"live\""));
        assert!(!config.contains(LEGACY_PROXY_URL_FRAGMENT));
        assert!(!relay.exists());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn migration_prefers_actual_model_over_the_display_label() {
        // The relay file of a supplier that exposes a model under a friendly
        // label carries BOTH: `displayModel` is what the UI shows, and
        // `actualModel` is the id the provider actually serves. Migrating on
        // the label instead is what made Codex send `gpt-5.5` to a gateway
        // with no such channel ("no available channel", HTTP 503). The
        // migration must key off the real id and leave no trace of the label.
        let dir = unique_tmpdir();
        let relay = dir.join(LEGACY_RELAY_FILENAME);
        fs::write(
            dir.join(CONFIG_FILENAME),
            "model = \"gpt-5.5\"\n\n[model_providers.OpenAI]\nbase_url = \"http://127.0.0.1:53682/v1\"\n",
        )
        .unwrap();
        fs::write(
            &relay,
            serde_json::json!({
                "apiKey": "test-key",
                "baseUrl": "https://api.pie-xian.com/v1",
                "actualModel": "space-bunny-free",
                "displayModel": "gpt-5.5",
                "modelName": "小随管"
            })
            .to_string(),
        )
        .unwrap();

        assert!(migrate_legacy_proxy_config_from(&dir, &relay).unwrap());
        let config = fs::read_to_string(dir.join(CONFIG_FILENAME)).unwrap();
        assert!(
            config.contains("model = \"space-bunny-free\""),
            "real model id missing: {config}"
        );
        assert!(
            !config.contains("gpt-5.5"),
            "the display label leaked into the migrated config: {config}"
        );
        assert!(config.contains("base_url = \"https://api.pie-xian.com/v1\""));
        assert!(!config.contains(LEGACY_PROXY_URL_FRAGMENT));
        fs::remove_dir_all(dir).ok();
    }

    /// The deleted proxy's `responsesPassthrough` flag is the predecessor of
    /// the protocol choice, and the two states it could be in have to survive
    /// the migration on opposite sides: `false` meant "translate Responses to
    /// Chat", which is what the no-choice protocol now does, and `true` meant
    /// the gateway really does serve `/v1/responses` and wants the direct
    /// path. Losing the second case would silently reroute a working
    /// Responses-native supplier through the bridge.
    #[test]
    fn migration_carries_responses_passthrough_into_the_protocol_choice() {
        let relay_json = |passthrough: bool| {
            serde_json::json!({
                "apiKey": "test-key",
                "baseUrl": "https://provider.example/v1",
                "actualModel": "provider-model",
                "displayModel": "gpt-5.5",
                "modelName": "supplier",
                "relayMode": false,
                "responsesPassthrough": passthrough
            })
            .to_string()
        };
        let legacy_config = "model = \"gpt-5.5\"\n\n[model_providers.OpenAI]\nbase_url = \"http://127.0.0.1:53682/v1\"\n";

        // Passthrough off: the historical default, so the bridged Chat path.
        let off = unique_tmpdir();
        let off_relay = off.join(LEGACY_RELAY_FILENAME);
        fs::write(off.join(CONFIG_FILENAME), legacy_config).unwrap();
        fs::write(&off_relay, relay_json(false)).unwrap();
        assert!(migrate_legacy_proxy_config_from(&off, &off_relay).unwrap());
        let off_config = fs::read_to_string(off.join(CONFIG_FILENAME)).unwrap();
        assert!(
            off_config.contains(&format!(
                "base_url = \"{}/v1\"",
                crate::services::protocol_bridge::base_url()
            )),
            "passthrough=false must migrate to the bridge: {off_config}"
        );
        assert!(!off_config.contains("https://provider.example/v1"));
        fs::remove_dir_all(off).ok();

        // Passthrough on: the gateway speaks Responses, so keep it direct.
        let on = unique_tmpdir();
        let on_relay = on.join(LEGACY_RELAY_FILENAME);
        fs::write(on.join(CONFIG_FILENAME), legacy_config).unwrap();
        fs::write(&on_relay, relay_json(true)).unwrap();
        assert!(migrate_legacy_proxy_config_from(&on, &on_relay).unwrap());
        let on_config = fs::read_to_string(on.join(CONFIG_FILENAME)).unwrap();
        assert!(
            on_config.contains("base_url = \"https://provider.example/v1\""),
            "passthrough=true must keep the direct Responses path: {on_config}"
        );
        assert!(!on_config.contains("gpt-5.5"));
        fs::remove_dir_all(on).ok();
    }
}
