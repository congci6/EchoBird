//! Codex and ChatGPT launch support.
//!
//! Model traffic does still pass through EchoBird. Model application points
//! `~/.codex/config.toml` at the protocol bridge, not at the provider: Codex
//! sends the model label it remembers rather than the `model` in that file, so
//! a config aimed straight at the provider sends a label the provider has no
//! channel for and gets `503 no available channel for model gpt-5.5` back.
//! The bridge pins the real id and converts the dialect as needed.

mod codex_binary;
mod config_manager;
mod onboarding_bypass;

pub use codex_binary::{
    resolve_codex_cli_binary, resolve_codex_cli_shim, resolve_desktop_binary,
    resolve_desktop_launch_uri, resolve_desktop_launch_uri_scanned,
};
pub use config_manager::{default_codex_dir, migrate_legacy_proxy_config};
pub use onboarding_bypass::bypass_onboarding;
