//! Device-local Pi runtime configuration exposed through Nova settings.
//!
//! Secrets never cross the RPC boundary: provider rows report only the
//! credential source/type. Settings and packages retain Pi's native global vs
//! project scope instead of being synchronized through the workspace doc.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PiSettingsScope {
    Global,
    Project,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiRuntimeInfo {
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    pub config_dir: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiCommonSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_thinking_level: Option<String>,
    pub auto_compaction: bool,
    pub auto_retry: bool,
    pub default_project_trust: String,
    pub transport: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiProviderStatus {
    pub id: String,
    pub name: String,
    pub configured: bool,
    /// Human-readable source without credential material: Stored OAuth,
    /// Stored API key, environment variable, or custom provider.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_variable: Option<String>,
    #[serde(default)]
    pub custom: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiOpenAiCompatibleStatus {
    pub configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    pub has_stored_key: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiPackageInfo {
    pub source: String,
    pub scope: PiSettingsScope,
    pub kind: String,
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiResourceInfo {
    pub kind: String,
    pub path: String,
    pub scope: PiSettingsScope,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiSettingsSnapshot {
    pub runtime: PiRuntimeInfo,
    pub scope: PiSettingsScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    pub settings: PiCommonSettings,
    pub providers: Vec<PiProviderStatus>,
    pub openai_compatible: PiOpenAiCompatibleStatus,
    pub packages: Vec<PiPackageInfo>,
    pub resources: Vec<PiResourceInfo>,
    pub global_settings_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_settings_path: Option<String>,
    pub auth_path: String,
    pub models_path: String,
    /// Pretty-printed effective settings for diagnostics. This contains no
    /// credentials; auth.json is deliberately never returned.
    pub effective_settings_json: String,
}
