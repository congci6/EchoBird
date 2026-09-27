// Model configuration entry points and shared file-format helpers.
// Tool-specific implementations live in tool_config_manager/<tool>.rs.

mod aider;
mod claudecode;
mod claudedesktop;
mod codex;
pub(crate) mod dsh;
mod generic;
mod grok;
mod kilo;
mod kimicode;
mod mimocode;
mod mimodesktop;
mod omp;
mod openclaw;
mod opencode;
mod openscience;
mod pi;
mod proxy_ports;
mod qwencode;
mod relay;
mod vibe_trading;
mod workbuddy;
mod zcode;

use crate::services::protocol::WireProtocol;
use crate::services::{codex_accounts, tool_manager};
use aider::{apply_aider, read_aider};
use claudecode::{
    apply_claudecode, normalize_model_info_for_tool, read_claudecode,
    restore_claudecode_to_official,
};
use claudedesktop::{apply_claudedesktop, read_claudedesktop, restore_claudedesktop_to_official};
pub(crate) use codex::{apply_codex, apply_codex_at};
use codex::{read_codex, restore_codex_to_official};
use dsh::{apply_dsh, read_dsh, restore_dsh_to_official};
use generic::{apply_generic_json, read_generic_json};
use grok::{apply_grok, read_grok, restore_grok_to_official};
pub use kilo::kilo_echobird_model;
use kilo::{apply_kilo, read_kilo, restore_kilo_to_official};
use kimicode::{
    apply_kimicode, apply_kimidesktop, read_kimicode, read_kimidesktop,
    restore_kimicode_to_official, restore_kimidesktop_to_official,
};
pub use mimocode::mimocode_echobird_model;
use mimocode::{apply_mimocode, read_mimocode, restore_mimocode_to_official};
use openclaw::{apply_openclaw, read_openclaw};
use opencode::{apply_opencode, read_opencode, restore_opencode_to_official};
use openscience::{apply_openscience, read_openscience, restore_openscience_to_official};
use pi::{apply_pi, read_pi, restore_pi_to_official};
pub(crate) use proxy_ports::migrate_local_proxy_port;
use qwencode::{apply_qwen_code, read_qwen_code};
use relay::{apply_echobird_relay, read_echobird_relay};
use std::fs;
use std::path::{Path, PathBuf};
use vibe_trading::{apply_vibe_trading, read_vibe_trading};
use workbuddy::{apply_workbuddy, read_workbuddy};
use zcode::{apply_zcode, read_zcode, restore_zcode_to_official};

/// Model info to apply to a tool
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anthropic_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Which wire protocol the user wants this model used through, when it
    /// differs from what the provider serves natively. EchoBird then runs the
    /// protocol bridge to translate. Absent means "use the provider directly",
    /// which is the behaviour every config written before protocol selection
    /// existed relies on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_protocol: Option<String>,
    /// Responses only. Set when the provider is known NOT to serve
    /// `/v1/responses`, which is a common shape for OpenAI-compatible gateways
    /// — they answer `not implemented` rather than `404`, so nothing before the
    /// request can reveal it.
    ///
    /// A Responses choice is otherwise taken at face value, because the
    /// canonical round trip has no room for `previous_response_id`, `store` or
    /// `include` and a supplier that does serve Responses would silently lose
    /// exactly the state Codex depends on. Turning this on degrades the choice
    /// to Chat Completions, so the bridge translates instead. It affects no
    /// other protocol, and absent means "off", so every config written before
    /// the switch existed keeps its behaviour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub responses_fallback: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_model: Option<String>,
    /// Claude Desktop / Claude Code only. Connect directly to the selected
    /// Anthropic-compatible relay instead of EchoBird's model-id router.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_mode: Option<bool>,
    /// Claude Code relay-only. When `Some(true)` AND `relay_mode` is on,
    /// append `[1m]` to the 1M-capable env vars (`ANTHROPIC_MODEL` /
    /// `ANTHROPIC_DEFAULT_SONNET_MODEL` / `ANTHROPIC_DEFAULT_OPUS_MODEL` / `ANTHROPIC_DEFAULT_FABLE_MODEL`)
    /// written to ~/.claude/settings.json so Claude Code budgets the 1M
    /// context window. Claude Code strips the suffix before sending the id
    /// upstream, so the provider still sees the bare id. `HAIKU` and
    /// `CLAUDE_CODE_SUBAGENT_MODEL` never get the suffix — no 1M concept.
    /// No effect in bridge mode (bridge writes no model id — CC uses its
    /// built-in claude-* ids, which already budget the full window).
    /// Only consumed by `apply_claudecode`; other tools ignore it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub one_m_context: Option<bool>,
}

/// Result of applying a model config
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApplyResult {
    pub success: bool,
    pub message: String,
}

// ─── Helpers ───

fn echobird_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".echobird")
}

fn ensure_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            let _ = fs::create_dir_all(parent);
        }
    }
}

/// Extract domain name from URL for use in identifiers
/// Example: "https://api.openai.com/v1" -> "api_openai_com"
fn extract_domain_name(url: &str) -> String {
    url.trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or(url)
        .split(':')
        .next()
        .unwrap_or(url)
        .replace('.', "_")
}

/// Read JSON file, return Value or None
fn read_json_file(path: &Path) -> Option<serde_json::Value> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_jsonc_file(path: &Path) -> Option<serde_json::Value> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&strip_jsonc_comments(&content)).ok()
}

fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(c) = chars.next() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            out.push(c);
            continue;
        }

        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }

        if c == '/' {
            match chars.peek().copied() {
                Some('/') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                    continue;
                }
                Some('*') => {
                    chars.next();
                    let mut prev = '\0';
                    for next in chars.by_ref() {
                        if prev == '*' && next == '/' {
                            break;
                        }
                        prev = next;
                    }
                    continue;
                }
                _ => {}
            }
        }

        out.push(c);
    }

    out
}

/// Write JSON value to file with pretty formatting
fn write_json_file(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    ensure_parent(path);
    let content = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| format!("Failed to write {}: {}", path.display(), e))
}

/// `serde_yaml_ng` shorthand: a string value.
fn yaml_str(s: &str) -> serde_yaml_ng::Value {
    serde_yaml_ng::Value::String(s.to_string())
}

/// `serde_yaml_ng` shorthand: an empty mapping value.
fn yaml_map() -> serde_yaml_ng::Value {
    serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new())
}

/// Coerce a YAML value into a mapping, returning `&mut Mapping`. Replaces a
/// non-mapping value with a fresh mapping — the YAML analogue of the jsonc
/// guards used by the openscience/zcode configs.
fn yaml_as_map_mut(value: &mut serde_yaml_ng::Value) -> &mut serde_yaml_ng::Mapping {
    if !value.is_mapping() {
        *value = yaml_map();
    }
    value.as_mapping_mut().expect("coerced to mapping")
}

/// Get-or-create a child mapping under `key` in `map`, returning `&mut Mapping`.
fn yaml_child_map<'a>(
    map: &'a mut serde_yaml_ng::Mapping,
    key: &str,
) -> &'a mut serde_yaml_ng::Mapping {
    let child = map.entry(yaml_str(key)).or_insert_with(yaml_map);
    yaml_as_map_mut(child)
}

/// Read a child value from a mapping by string key (None if absent/non-mapping).
fn yaml_get<'a>(value: &'a serde_yaml_ng::Value, key: &str) -> Option<&'a serde_yaml_ng::Value> {
    value.as_mapping()?.get(yaml_str(key))
}

/// Write a `serde_yaml_ng::Value` back to a YAML file (block style), creating
/// parent directories as needed. Mirrors `write_json_file`.
fn write_yaml_file(path: &Path, value: &serde_yaml_ng::Value) -> Result<(), String> {
    ensure_parent(path);
    let content = serde_yaml_ng::to_string(value).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| format!("Failed to write {}: {}", path.display(), e))
}

/// Look up a model's real input modalities. Returns `["text"]` for models the
/// registry does not list so ZCode's `modalities.input` keeps the safe
/// text-only default for unknown ids.
fn model_input_modalities_for(model_id: &str) -> &'static [&'static str] {
    match model_id {
        "MiniMax-M3" => &["text", "image", "video"],
        _ => &["text"],
    }
}

// ════════════════════════════════════════════════════════════════
//  APPLY MODEL �?main entry point
// ════════════════════════════════════════════════════════════════

/// The path a tool's own config is given so its request lands on the bridge's
/// route for `client_protocol`.
///
/// The bridge serves all four dialects from one port, so for three of them a
/// plain OpenAI-style base is enough: the tool appends `/chat/completions` or
/// `/responses` or `/messages` itself, exactly as it would for a real
/// provider. Gemini is the exception — it addresses the model inside the path
/// (`/v1beta/models/{model}:generateContent`), so handing the tool `/v1` would
/// point it at a collection endpoint and drop the model on the floor.
fn bridge_client_base(client_protocol: WireProtocol) -> &'static str {
    if client_protocol.model_in_path() {
        "/v1beta"
    } else {
        "/v1"
    }
}

/// Point a tool at the local protocol bridge when the model is pinned to a
/// dialect that tool cannot speak itself.
///
/// Nearly every tool in the catalogue drives OpenAI Chat Completions through
/// its `base_url`. When the user pins the model to a different dialect we must
/// not write the upstream URL into that field — the tool would call, say,
/// `/v1/chat/completions` on a Responses-only endpoint and get a 404. Writing
/// the bridge's loopback URL there instead lets EchoBird translate in both
/// directions, which is what makes the choice a free one.
///
/// Only an explicit choice can require this. With no `api_protocol` every tool
/// stays on the dialect it already speaks — the behaviour every config written
/// before protocol selection existed relies on.
fn route_through_bridge_if_needed(tool_id: &str, model_info: &mut ModelInfo) {
    // Codex and the Claude apps run their own proxies/relays and decide their
    // own protocol (codex.rs makes its own Responses-vs-bridge call). Re-
    // pointing their base URL here would fight that machinery.
    if matches!(
        tool_id,
        "codex" | "chatgptdesktop" | "claudecode" | "claudedesktop"
    ) {
        return;
    }

    let Some(selected) = model_info
        .api_protocol
        .as_deref()
        .and_then(WireProtocol::parse)
    else {
        return;
    };
    if selected == WireProtocol::OpenaiChat {
        return;
    }

    // Only tools that speak OpenAI through `base_url` are safe to re-point
    // here. An Anthropic-only tool keeps its URL in `anthropic_url` and has
    // its own relay; a tool declaring nothing is one we know nothing about.
    let speaks_openai = tool_manager::tool_api_protocols(tool_id)
        .is_some_and(|protocols| protocols.iter().any(|entry| entry == "openai"));
    if !speaks_openai {
        return;
    }

    let Some(upstream) = model_info
        .base_url
        .clone()
        .filter(|url| !url.trim().is_empty())
    else {
        log::warn!("[ProtocolBridge] {tool_id} has no base URL to bridge through");
        return;
    };

    // The upstream dialect is the one the PROVIDER speaks, which is not
    // necessarily the one the user picked. `native_endpoint` is the same
    // mapping codex.rs uses, kept identical on purpose: writing `selected`
    // straight into the target makes the choice a no-op, because the bridge
    // would then forward e.g. a Gemini request to a supplier serving only Chat
    // Completions and answer 404 instead of translating anything.
    let (native, native_base) = codex::native_endpoint(model_info, &upstream, selected);

    crate::services::protocol_bridge::ensure_serving(
        crate::services::protocol_bridge::BridgeTarget {
            base_url: native_base,
            api_key: model_info.api_key.clone().unwrap_or_default(),
            model: model_info.model.clone().unwrap_or_default(),
            protocol: native,
        },
    );
    model_info.base_url = Some(format!(
        "{}{}",
        crate::services::protocol_bridge::base_url(),
        bridge_client_base(selected)
    ));
    log::info!(
        "[ProtocolBridge] {tool_id} routed through the bridge as {}",
        selected.as_str()
    );
}

pub async fn apply_model_to_tool(tool_id: &str, model_info: ModelInfo) -> ApplyResult {
    log::info!("[ToolConfigManager] Applying model to {}", tool_id);
    let mut model_info = normalize_model_info_for_tool(tool_id, model_info);
    route_through_bridge_if_needed(tool_id, &mut model_info); // Dispatch custom tools to their own handlers
    match tool_id {
        // OpenClaw: direct write to ~/.openclaw/openclaw.json (no patch needed since v2026.3.13)
        "openclaw" => return apply_openclaw(&model_info),

        // Type 3: Direct JSON overwrite (special format).
        // CLI and Desktop share ~/.config/opencode/opencode.jsonc — one apply
        // covers both (Desktop spawns `opencode serve` reading the same file).
        "opencode" | "opencodedesktop" => return apply_opencode(&model_info),

        // MiMo Code (Xiaomi fork of OpenCode): same provider schema,
        // own config at ~/.config/mimocode/mimocode.json(c).
        "mimocode" => return apply_mimocode(&model_info),
        "mimodesktop" => return mimodesktop::apply(&model_info),
        "kimidesktop" => return apply_kimidesktop(&model_info),

        // Kilo Code (Kilo fork of OpenCode): same provider schema,
        // own config at ~/.config/kilo/kilo.json.
        "kilo" => return apply_kilo(&model_info),

        // OpenScience (open-source Claude Science alt): models.dev provider
        // schema, dual-protocol (npm @ai-sdk/anthropic | @ai-sdk/openai-compatible),
        // config at ~/.config/openscience/openscience.json.
        "openscience" => return apply_openscience(&model_info),
        "dsh" => return apply_dsh(&model_info),

        // ZCode (Z.AI desktop OpenCode fork): OpenCode schema but the provider
        // uses a `kind` discriminator and supports BOTH protocols; config at
        // ~/.zcode/v2/config.json.
        "zcode" => return apply_zcode(&model_info),

        // Codex CLI and ChatGPT desktop share ~/.codex/config.toml.
        "codex" | "chatgptdesktop" => return apply_codex(tool_id, &model_info),

        // Claude Desktop 3P profile (Anthropic-native providers only)
        "claudedesktop" => return apply_claudedesktop(&model_info),

        // Claude Code — same model-id-rewrite proxy path as Claude Desktop,
        // but writes ~/.claude/settings.json env vars + its own relay file.
        "claudecode" => return apply_claudecode(&model_info),

        // Type 4: YAML
        "aider" => return apply_aider(&model_info),

        // Grok Build CLI (xAI) — sectioned TOML with [model.echobird] + [models]
        "grok" => return apply_grok(&model_info),

        // Qwen Code: direct write to ~/.qwen/settings.json
        "qwencode" => return apply_qwen_code(&model_info),

        // Pi (earendil-works/pi): writes ~/.pi/agent/{models,settings}.json
        "pi" => return apply_pi(&model_info),
        "omp" => return omp::apply(&model_info),

        // Kimi CLI (Moonshot AI): TOML shared with Desktop at ~/.kimi-code/config.toml
        "kimicode" => return apply_kimicode(&model_info),

        // Vibe-Trading (HKUDS): dotenv at ~/.vibe-trading/.env. Every endpoint
        // we point it at is OpenAI-compatible, so pin LANGCHAIN_PROVIDER=openai.
        "vibe-trading" => return apply_vibe_trading(&model_info),

        // WorkBuddy (Tencent CodeBuddy 办公版): ~/.workbuddy/models.json.
        "workbuddy" | "workbuddyai" => return apply_workbuddy(tool_id, &model_info),

        // Plug-and-play: check config.json custom flag
        _ => {
            if let Some((def, _)) = tool_manager::get_tool_config_mapping(tool_id) {
                if def.config_mapping.custom {
                    return apply_echobird_relay(tool_id, &model_info, false);
                }
            }
        }
    }

    apply_generic_json(tool_id, &model_info).await
}

// ════════════════════════════════════════════════════════════════
//  RESTORE TO OFFICIAL — delete config so tool regenerates defaults
// ════════════════════════════════════════════════════════════════

/// Delete the tool's config file (and any Echobird relay side-channel) so
/// the tool itself regenerates a fresh, vendor-default config on next launch.
/// Used by the App Desktop "restore to official" flow.
pub async fn restore_tool_to_official(tool_id: &str) -> ApplyResult {
    let config_path = match tool_manager::get_tool_config_mapping(tool_id) {
        Some((_, path)) => path,
        None => {
            return ApplyResult {
                success: false,
                message: format!("Unknown tool: {}", tool_id),
            }
        }
    };

    if matches!(tool_id, "codex" | "chatgptdesktop") {
        let codex_config_path = codex_accounts::codex_home().unwrap_or_default();
        return restore_codex_to_official(tool_id, &codex_config_path.join("config.toml"));
    }
    if tool_id == "claudedesktop" {
        return restore_claudedesktop_to_official();
    }
    if tool_id == "claudecode" {
        return restore_claudecode_to_official();
    }
    if tool_id == "grok" {
        return restore_grok_to_official();
    }
    if matches!(tool_id, "opencode" | "opencodedesktop") {
        return restore_opencode_to_official();
    }
    if tool_id == "mimocode" {
        return restore_mimocode_to_official();
    }
    if tool_id == "mimodesktop" {
        return mimodesktop::restore();
    }
    if tool_id == "kimidesktop" {
        return restore_kimidesktop_to_official();
    }
    if tool_id == "kilo" {
        return restore_kilo_to_official();
    }
    if tool_id == "zcode" {
        return restore_zcode_to_official();
    }
    if tool_id == "pi" {
        return restore_pi_to_official();
    }
    if tool_id == "omp" {
        return omp::restore();
    }
    if tool_id == "kimicode" {
        return restore_kimicode_to_official();
    }
    if tool_id == "openscience" {
        return restore_openscience_to_official();
    }
    if tool_id == "dsh" {
        return restore_dsh_to_official();
    }

    // Side-channel relay file (openclaw and other "custom" tools) —
    // best-effort cleanup, ignored if absent.
    let relay_path = echobird_dir().join(format!("{}.json", tool_id));
    if relay_path.exists() {
        let _ = fs::remove_file(&relay_path);
    }

    if !config_path.exists() {
        return ApplyResult {
            success: true,
            message: format!(
                "{} already at defaults — no config file to remove.",
                tool_id
            ),
        };
    }

    match fs::remove_file(&config_path) {
        Ok(_) => {
            log::info!(
                "[ToolConfigManager] Restored {} — deleted {:?}",
                tool_id,
                config_path
            );
            ApplyResult {
                success: true,
                message: format!(
                    "{} restored — config deleted, tool will regenerate defaults on next launch.",
                    tool_id
                ),
            }
        }
        Err(e) => ApplyResult {
            success: false,
            message: format!("Failed to delete {} config: {}", tool_id, e),
        },
    }
}

// ════════════════════════════════════════════════════════════════
//  GET MODEL INFO �?main entry point
// ════════════════════════════════════════════════════════════════

pub async fn get_tool_model_info(tool_id: &str) -> Option<ModelInfo> {
    match tool_id {
        "openclaw" => return read_openclaw(),
        "opencode" | "opencodedesktop" => return read_opencode(),
        "mimocode" => return read_mimocode(),
        "mimodesktop" => return mimodesktop::read(),
        "kimidesktop" => return read_kimidesktop(),
        "kilo" => return read_kilo(),
        "openscience" => return read_openscience(),
        "dsh" => return read_dsh(),
        "zcode" => return read_zcode(),
        "codex" | "chatgptdesktop" => return read_codex(),
        "claudedesktop" => return read_claudedesktop(),
        "claudecode" => return read_claudecode(),
        "aider" => return read_aider(),
        "grok" => return read_grok(),
        "qwencode" => return read_qwen_code(),
        "pi" => return read_pi(),
        "omp" => return omp::read(),
        "kimicode" => return read_kimicode(),
        "vibe-trading" => return read_vibe_trading(),
        "workbuddy" | "workbuddyai" => return read_workbuddy(tool_id),
        // Plug-and-play: check config.json custom flag
        _ => {
            if let Some((def, _)) = tool_manager::get_tool_config_mapping(tool_id) {
                if def.config_mapping.custom {
                    return read_echobird_relay(tool_id);
                }
            }
        }
    }

    read_generic_json(tool_id)
}

// ════════════════════════════════════════════════════════════════
//  Simple TOML helpers (top-level key = "value" only)
// ════════════════════════════════════════════════════════════════

pub(crate) fn toml_read_top(content: &str, key: &str) -> String {
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') || t.starts_with('#') || t.is_empty() {
            if t.starts_with('[') {
                break;
            } // Entered sections, stop
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            if k.trim() == key {
                let v = v.trim();
                if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
                    return v[1..v.len() - 1].to_string();
                }
                return v.to_string();
            }
        }
    }
    String::new()
}

/// Surgically remove a top-level `key = ...` line from a TOML document,
/// preserving every other line and section verbatim. Mirrors the
/// top-level-only scan of `toml_write_top`: only keys before the first
/// `[section]` are candidates (the write helpers never touch keys
/// inside a section, so a top-level key is the only shape we'd ever
/// need to delete). No-op if the key is absent. The trailing-newline
/// convention is re-applied by `write_codex_canonical_fields` at the
/// end of its pipeline, so this helper — like `toml_write_top` — does
/// not re-add it.
///
/// Used to evict legacy keys we no longer write (e.g. `review_model`)
/// so a stale value left by an older EchoBird version can't survive a
/// model switch / pre-spawn self-heal.
fn toml_delete_top(content: &str, key: &str) -> String {
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let mut first_section: Option<usize> = None;
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if first_section.is_none() && t.starts_with('[') {
            first_section = Some(i);
        }
        // Once we've entered the first [section], the remaining top-level
        // keys are exhausted — stop scanning (a same-named key inside a
        // section belongs to that section, not the top level).
        if first_section.is_some() && i >= first_section.unwrap() {
            break;
        }
        if let Some((k, _)) = t.split_once('=') {
            if k.trim() == key {
                lines.remove(i);
                break;
            }
        }
        i += 1;
    }
    lines.join("\n")
}

fn toml_write_top(content: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let mut found = false;
    let mut first_section: Option<usize> = None;

    for (i, line) in lines.iter_mut().enumerate() {
        let t = line.trim();
        if first_section.is_none() && t.starts_with('[') {
            first_section = Some(i);
        }
        if first_section.is_some() && i >= first_section.unwrap() {
            continue;
        }
        if let Some((k, _)) = t.split_once('=') {
            if k.trim() == key {
                *line = format!("{} = \"{}\"", key, toml_escape(value));
                found = true;
                break;
            }
        }
    }

    if !found {
        let new_line = format!("{} = \"{}\"", key, toml_escape(value));
        match first_section {
            Some(i) => lines.insert(i, new_line),
            None => lines.push(new_line),
        }
    }
    lines.join("\n")
}

/// Variant of `toml_write_top` that writes the value verbatim, without
/// wrapping it in `"..."`. Use for booleans (`true`/`false`) and integers
/// — TOML rejects them when quoted. Mirrors the line-based, overwrite-
/// or-insert semantics of the string variant.
fn toml_write_top_raw(content: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let mut found = false;
    let mut first_section: Option<usize> = None;

    for (i, line) in lines.iter_mut().enumerate() {
        let t = line.trim();
        if first_section.is_none() && t.starts_with('[') {
            first_section = Some(i);
        }
        if first_section.is_some() && i >= first_section.unwrap() {
            continue;
        }
        if let Some((k, _)) = t.split_once('=') {
            if k.trim() == key {
                *line = format!("{} = {}", key, value);
                found = true;
                break;
            }
        }
    }

    if !found {
        let new_line = format!("{} = {}", key, value);
        match first_section {
            Some(i) => lines.insert(i, new_line),
            None => lines.push(new_line),
        }
    }
    lines.join("\n")
}

/// Surgically write `key = "value"` inside `[table]` of a TOML
/// document, preserving every other line and section verbatim. If the
/// table doesn't exist, append it at end-of-file. If the key doesn't
/// exist inside the table, insert it just after the table header.
/// Mirrors `toml_write_top` line-based semantics — no full parse, no
/// reformatting, no comment loss. Used by `apply_codex` to canonicalize
/// `[model_providers.OpenAI]` fields without clobbering Codex's own
/// runtime state (`[projects.*]` trust, `[tui.*]` NUX, etc.) that sits
/// in the same file.
pub(crate) fn toml_write_table_value(content: &str, table: &str, key: &str, value: &str) -> String {
    let header = format!("[{}]", table);
    let mut lines: Vec<String> = content.lines().map(String::from).collect();

    let table_start = lines.iter().position(|l| l.trim() == header.as_str());

    let table_start = match table_start {
        Some(i) => i,
        None => {
            // Table missing — append. Pad with a blank line if the
            // existing file doesn't already end with one.
            if !lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
                lines.push(String::new());
            }
            lines.push(header);
            lines.push(format!("{} = \"{}\"", key, toml_escape(value)));
            return lines.join("\n");
        }
    };

    // Find table's end (next section header or EOF).
    let table_end = lines
        .iter()
        .enumerate()
        .skip(table_start + 1)
        .find_map(|(i, l)| {
            let t = l.trim();
            if t.starts_with('[') && t.ends_with(']') {
                Some(i)
            } else {
                None
            }
        })
        .unwrap_or(lines.len());

    // Look for the key inside the table's range.
    let key_line = (table_start + 1..table_end).find(|&i| {
        let t = lines[i].trim();
        if t.starts_with('#') || t.is_empty() {
            return false;
        }
        match t.split_once('=') {
            Some((k, _)) => k.trim() == key,
            None => false,
        }
    });

    let replacement = format!("{} = \"{}\"", key, toml_escape(value));
    match key_line {
        Some(i) => lines[i] = replacement,
        None => lines.insert(table_start + 1, replacement),
    }

    lines.join("\n")
}

/// Variant of `toml_write_table_value` that writes the value verbatim,
/// without wrapping it in `"..."`. For booleans / integers inside a
/// table (e.g. `requires_openai_auth = true`). Same surgical line-based
/// semantics; same preservation of unrelated sections.
fn toml_write_table_value_raw(content: &str, table: &str, key: &str, value: &str) -> String {
    let header = format!("[{}]", table);
    let mut lines: Vec<String> = content.lines().map(String::from).collect();

    let table_start = lines.iter().position(|l| l.trim() == header.as_str());

    let table_start = match table_start {
        Some(i) => i,
        None => {
            if !lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
                lines.push(String::new());
            }
            lines.push(header);
            lines.push(format!("{} = {}", key, value));
            return lines.join("\n");
        }
    };

    let table_end = lines
        .iter()
        .enumerate()
        .skip(table_start + 1)
        .find_map(|(i, l)| {
            let t = l.trim();
            if t.starts_with('[') && t.ends_with(']') {
                Some(i)
            } else {
                None
            }
        })
        .unwrap_or(lines.len());

    let key_line = (table_start + 1..table_end).find(|&i| {
        let t = lines[i].trim();
        if t.starts_with('#') || t.is_empty() {
            return false;
        }
        match t.split_once('=') {
            Some((k, _)) => k.trim() == key,
            None => false,
        }
    });

    let replacement = format!("{} = {}", key, value);
    match key_line {
        Some(i) => lines[i] = replacement,
        None => lines.insert(table_start + 1, replacement),
    }

    lines.join("\n")
}

pub(crate) fn toml_read_table_value(content: &str, table: &str, key: &str) -> String {
    let header = format!("[{}]", table);
    let mut in_table = false;

    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            in_table = t == header;
            continue;
        }
        if !in_table || t.starts_with('#') || t.is_empty() {
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            if k.trim() == key {
                return toml_unquote(v.trim());
            }
        }
    }

    String::new()
}

fn toml_unquote(value: &str) -> String {
    let v = value.trim();
    if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
        v[1..v.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        v.to_string()
    }
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_delete_top_removes_a_top_level_key() {
        // The key we own sits before the first [section]: delete it, keep
        // the rest verbatim (whitespace, comments, sections all untouched).
        let content = "model_provider = \"OpenAI\"\n\
                       model = \"gpt-5.5\"\n\
                       review_model = \"gpt-5.5\"\n\
                       \n\
                       [model_providers.OpenAI]\n\
                       name = \"OpenAI\"\n";
        let out = toml_delete_top(content, "review_model");
        assert!(!out.contains("review_model"));
        assert!(out.contains("model = \"gpt-5.5\""));
        assert!(out.contains("[model_providers.OpenAI]"));
        assert!(out.contains("name = \"OpenAI\""));
    }

    #[test]
    fn toml_delete_top_is_noop_when_key_absent() {
        // No key removed → only the join-strips-trailing-newline behavior
        // of `content.lines().collect().join("\n")` changes the string (the
        // caller, write_codex_canonical_fields, re-adds it). Assert the scan
        // matched nothing by checking the lines content survives.
        let content = "model = \"gpt-5.5\"\n[model_providers.OpenAI]\n";
        let out = toml_delete_top(content, "review_model");
        assert!(out.contains("model = \"gpt-5.5\""));
        assert!(out.contains("[model_providers.OpenAI]"));
        assert!(!out.contains("review_model"));
    }

    #[test]
    fn toml_delete_top_ignores_same_named_key_inside_a_section() {
        // A `review_model` that lives INSIDE a [section] belongs to that
        // section — not a top-level key we own — so it must survive. Only
        // the pre-section scan should match.
        let content = "model = \"gpt-5.5\"\n\
                       [model_providers.OpenAI]\n\
                       review_model = \"leave-me\"\n";
        let out = toml_delete_top(content, "review_model");
        assert!(out.contains("review_model = \"leave-me\""));
        assert!(out.contains("model = \"gpt-5.5\""));
    }

    // ── Per-model input modalities (ZCode) ──
    // apply_zcode must declare the real input modalities for a multimodal
    // model instead of forcing text-only.

    #[test]
    fn model_input_modalities_for_multimodal_model() {
        let m = model_input_modalities_for("MiniMax-M3");
        assert_eq!(m, &["text", "image", "video"]);
    }

    #[test]
    fn model_input_modalities_for_unknown_model_defaults_to_text() {
        assert_eq!(model_input_modalities_for("glm-5.2"), &["text"]);
    }

    // ─── protocol bridge routing ───
    //
    // Only the early-return arms are unit-tested here: the branch that
    // actually starts the bridge binds a port, so it is covered end-to-end by
    // `tests/protocol_conversion.rs` instead.

    fn routing_model(protocol: Option<&str>) -> ModelInfo {
        ModelInfo {
            name: Some("M".to_string()),
            model: Some("m".to_string()),
            base_url: Some("https://provider.example/v1".to_string()),
            api_key: Some("k".to_string()),
            anthropic_url: None,
            protocol: Some("openai".to_string()),
            api_protocol: protocol.map(str::to_string),
            display_model: None,
            relay_mode: None,
            one_m_context: None,
            responses_fallback: None,
        }
    }

    fn routing_model_with_fallback(protocol: Option<&str>, fallback: Option<bool>) -> ModelInfo {
        ModelInfo {
            responses_fallback: fallback,
            ..routing_model(protocol)
        }
    }

    /// Responses is the one dialect that cannot be verified up front: unlike
    /// Anthropic there is no `responses_url` to consult, and unlike Gemini the
    /// path shape is not distinctive, so the default is to take the provider at
    /// face value. That is right for a supplier that really serves
    /// `/v1/responses` — going through the canonical form would drop
    /// `previous_response_id`, `store` and `include` — and wrong for one that
    /// answers `not implemented`, which is a real and common shape for
    /// OpenAI-compatible gateways.
    ///
    /// The switch is how a user resolves that without EchoBird guessing: set
    /// it when the provider is known not to serve Responses, and the choice
    /// degrades to Chat Completions the same way an unconfigured Anthropic or
    /// Gemini choice does.
    #[test]
    fn the_responses_fallback_switch_degrades_a_responses_choice_to_chat() {
        let info = routing_model_with_fallback(Some("openai-responses"), Some(true));
        let target = resolved_target(&info).expect("resolvable");
        assert_eq!(
            target.protocol,
            WireProtocol::OpenaiChat,
            "with the switch on, a Responses choice must degrade to Chat"
        );
        assert_eq!(target.base_url, "https://provider.example/v1");
    }

    /// The switch is scoped to Responses. Turning it on must not change what
    /// any other choice resolves to, or a user leaving it on would silently
    /// downgrade a provider that does serve Responses properly.
    #[test]
    fn the_responses_fallback_switch_does_not_touch_other_protocols() {
        for picked in ["openai-chat", "anthropic", "gemini"] {
            let info = routing_model_with_fallback(Some(picked), Some(true));
            let target = resolved_target(&info).expect("resolvable");
            assert_eq!(
                target.protocol,
                resolved_target(&routing_model(Some(picked)))
                    .expect("resolvable")
                    .protocol,
                "{picked} must be unaffected by the Responses switch"
            );
        }
    }

    /// Off and absent both mean "take the provider at face value", so a config
    /// written before the switch existed keeps behaving exactly as it did.
    #[test]
    fn the_responses_fallback_switch_defaults_to_face_value() {
        for fallback in [None, Some(false)] {
            let info = routing_model_with_fallback(Some("openai-responses"), fallback);
            let target = resolved_target(&info).expect("resolvable");
            assert_eq!(
                target.protocol,
                WireProtocol::OpenaiResponses,
                "fallback={fallback:?} must keep Responses direct"
            );
        }
    }

    /// The bridge target a model resolves to, without starting a listener.
    ///
    /// `route_through_bridge_if_needed` has to bind a port, so the decision
    /// it makes is factored out here where it can be asserted directly. The
    /// single `BridgeTarget` is what the whole design turns on: the upstream
    /// protocol has to be the one the PROVIDER speaks, never the one the user
    /// picked, or no translation happens at all.
    fn resolved_target(
        model_info: &ModelInfo,
    ) -> Option<crate::services::protocol_bridge::BridgeTarget> {
        let selected = model_info
            .api_protocol
            .as_deref()
            .and_then(WireProtocol::parse)?;
        let upstream = model_info
            .base_url
            .clone()
            .filter(|url| !url.trim().is_empty())?;
        let (native, native_base) = codex::native_endpoint(model_info, &upstream, selected);
        Some(crate::services::protocol_bridge::BridgeTarget {
            base_url: native_base,
            api_key: model_info.api_key.clone().unwrap_or_default(),
            model: model_info.model.clone().unwrap_or_default(),
            protocol: native,
        })
    }

    /// A provider that has no dedicated endpoint for a dialect is reached on
    /// the OpenAI-compatible base, which every gateway serves.
    ///
    /// Responses is deliberately taken at face value — a supplier publishing
    /// `/v1/responses` keeps its fidelity through the bridge's verbatim
    /// passthrough, and `codex.rs` chose that trade-off on purpose. Anthropic
    /// falls back to Chat unless an Anthropic URL is configured. Gemini always
    /// falls back: `ModelInfo` has no Gemini base-URL field, so there is no way
    /// to tell a real Gemini supplier from a Chat-only one, and guessing Gemini
    /// gets a 404 instead of a translation.
    #[test]
    fn a_provider_without_a_dedicated_endpoint_is_reached_on_its_openai_base() {
        for (picked, expect_native) in [
            (WireProtocol::AnthropicMessages, WireProtocol::OpenaiChat),
            (
                WireProtocol::GeminiGenerateContent,
                WireProtocol::OpenaiChat,
            ),
            (WireProtocol::OpenaiResponses, WireProtocol::OpenaiResponses),
        ] {
            let info = routing_model(Some(picked.as_str()));
            let target = resolved_target(&info).expect("resolvable");
            assert_eq!(target.protocol, expect_native, "{picked:?}");
            assert_eq!(target.base_url, "https://provider.example/v1");
        }
    }

    /// A provider that DOES publish an Anthropic endpoint is reached as
    /// Anthropic, and on that endpoint rather than the OpenAI-compatible one.
    #[test]
    fn an_anthropic_choice_uses_the_configured_anthropic_endpoint() {
        let mut info = routing_model(Some("anthropic-messages"));
        info.anthropic_url = Some("https://anth.example/v1".to_string());
        let target = resolved_target(&info).expect("resolvable");
        assert_eq!(target.protocol, WireProtocol::AnthropicMessages);
        assert_eq!(target.base_url, "https://anth.example/v1");
    }

    /// Responses is a real endpoint on some gateways, so it is the one choice
    /// that may be taken at face value when the provider has no separate URL
    /// for it: a supplier serving `/v1/responses` keeps its fidelity through
    /// the bridge's verbatim passthrough.
    #[test]
    fn a_responses_choice_is_taken_at_face_value() {
        let info = routing_model(Some("openai-responses"));
        let target = resolved_target(&info).expect("resolvable");
        assert_eq!(target.protocol, WireProtocol::OpenaiResponses);
    }

    /// The bridge serves every dialect on one port, so the URL a tool is
    /// handed is the bridge's own base plus whatever the tool itself appends.
    /// Gemini is the exception: it addresses the model in the PATH, so a bare
    /// `/v1` would make the client call a collection endpoint and lose the
    /// model entirely.
    #[test]
    fn the_bridge_url_is_a_plain_base_except_for_gemini() {
        assert_eq!(bridge_client_base(WireProtocol::OpenaiChat), "/v1");
        assert_eq!(bridge_client_base(WireProtocol::OpenaiResponses), "/v1");
        assert_eq!(bridge_client_base(WireProtocol::AnthropicMessages), "/v1");
        assert_eq!(
            bridge_client_base(WireProtocol::GeminiGenerateContent),
            "/v1beta",
            "Gemini addresses the model in the path, so /v1 would drop it"
        );
    }

    #[test]
    fn no_protocol_choice_leaves_the_base_url_untouched() {
        // The pre-protocol-selection behaviour every existing config relies
        // on: nothing is rewritten, so nothing can start regressing.
        for protocol in [None, Some(""), Some("not-a-protocol")] {
            let mut info = routing_model(protocol);
            route_through_bridge_if_needed("opencode", &mut info);
            assert_eq!(
                info.base_url.as_deref(),
                Some("https://provider.example/v1"),
                "protocol={protocol:?} must not re-point the base URL"
            );
        }
    }

    #[test]
    fn chat_completions_choice_needs_no_bridge() {
        let mut info = routing_model(Some("openai-chat"));
        route_through_bridge_if_needed("opencode", &mut info);
        assert_eq!(
            info.base_url.as_deref(),
            Some("https://provider.example/v1")
        );
    }

    #[test]
    fn tools_with_their_own_routing_are_left_alone() {
        // Codex decides Responses-vs-bridge in codex.rs, and the Claude apps
        // have their own model-id relay. Re-pointing them here would fight
        // that machinery.
        for tool_id in ["codex", "chatgptdesktop", "claudecode", "claudedesktop"] {
            let mut info = routing_model(Some("gemini-generate-content"));
            route_through_bridge_if_needed(tool_id, &mut info);
            assert_eq!(
                info.base_url.as_deref(),
                Some("https://provider.example/v1"),
                "{tool_id} must keep its own routing"
            );
        }
    }

    #[test]
    fn tools_that_do_not_declare_openai_are_left_alone() {
        // An Anthropic-only tool keeps its URL in `anthropic_url`; re-pointing
        // `base_url` for it would not make it speak the chosen dialect.
        let mut info = routing_model(Some("openai-responses"));
        route_through_bridge_if_needed("claudescience", &mut info);
        assert_eq!(
            info.base_url.as_deref(),
            Some("https://provider.example/v1")
        );
    }
}
