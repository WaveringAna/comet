//! Device-local Pi runtime management for the native settings surface.
//!
//! Pi remains the owner of its file formats and package installation. Nova
//! reads a deliberately small typed projection, patches only named common
//! settings atomically, and shells out to the resolved Pi CLI for package
//! lifecycle commands. Credentials never leave this module.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use nova_proto::{
    PiCommonSettings, PiOpenAiCompatibleStatus, PiPackageInfo, PiProviderStatus, PiResourceInfo,
    PiRuntimeInfo, PiSettingsScope, PiSettingsSnapshot,
};
use serde_json::{Map, Value};
use tokio::process::Command;

#[derive(Debug, Clone, Default)]
pub struct PiManagement;

const PROVIDERS: &[(&str, &str, &str)] = &[
    ("anthropic", "Anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "OpenAI", "OPENAI_API_KEY"),
    ("openai-codex", "OpenAI Codex", ""),
    ("google", "Google Gemini", "GEMINI_API_KEY"),
    ("github-copilot", "GitHub Copilot", ""),
    ("openrouter", "OpenRouter", "OPENROUTER_API_KEY"),
    ("xai", "xAI", "XAI_API_KEY"),
    ("deepseek", "DeepSeek", "DEEPSEEK_API_KEY"),
    ("mistral", "Mistral", "MISTRAL_API_KEY"),
    ("groq", "Groq", "GROQ_API_KEY"),
    ("cerebras", "Cerebras", "CEREBRAS_API_KEY"),
    (
        "amazon-bedrock",
        "Amazon Bedrock",
        "AWS_BEARER_TOKEN_BEDROCK",
    ),
    (
        "vercel-ai-gateway",
        "Vercel AI Gateway",
        "AI_GATEWAY_API_KEY",
    ),
    (
        "cloudflare-ai-gateway",
        "Cloudflare AI Gateway",
        "CLOUDFLARE_API_KEY",
    ),
    ("huggingface", "Hugging Face", "HF_TOKEN"),
    ("fireworks", "Fireworks", "FIREWORKS_API_KEY"),
    ("together", "Together AI", "TOGETHER_API_KEY"),
    ("kimi-coding", "Kimi For Coding", "KIMI_API_KEY"),
    ("minimax", "MiniMax", "MINIMAX_API_KEY"),
    ("zai", "ZAI Coding Plan", "ZAI_API_KEY"),
    ("opencode", "OpenCode Zen", "OPENCODE_API_KEY"),
    ("radius", "Radius", "RADIUS_API_KEY"),
];

const OPENAI_COMPATIBLE_PROVIDER: &str = "openai-compatible";

impl PiManagement {
    pub fn config_dir(&self) -> PathBuf {
        std::env::var_os("PI_CODING_AGENT_DIR")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".pi").join("agent"))
    }

    pub async fn snapshot(
        &self,
        scope: PiSettingsScope,
        project_path: Option<&str>,
    ) -> Result<PiSettingsSnapshot, String> {
        let config_dir = self.config_dir();
        let global_settings_path = config_dir.join("settings.json");
        let auth_path = config_dir.join("auth.json");
        let models_path = config_dir.join("models.json");
        let project_root = validate_project_path(scope, project_path)?;
        let project_settings_path = project_root
            .as_ref()
            .map(|root| root.join(".pi").join("settings.json"));

        let global = read_object(&global_settings_path);
        let project = project_settings_path
            .as_ref()
            .map(|path| read_object(path))
            .unwrap_or_default();
        let effective = if scope == PiSettingsScope::Project {
            deep_merge(
                Value::Object(global.clone()),
                Value::Object(project.clone()),
            )
        } else {
            Value::Object(global.clone())
        };

        let executable = nova_harness::pi::resolve_pi_executable();
        let version = if let Some(executable) = executable.as_ref() {
            let output = Command::new(executable)
                .arg("--version")
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .await
                .ok();
            output.and_then(|out| {
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
                    .filter(|s| !s.is_empty())
            })
        } else {
            None
        };

        let auth = read_object(&auth_path);
        let models = read_object(&models_path);
        let providers = provider_rows(&auth, &models);
        let openai_compatible = openai_compatible_status(&auth, &models);
        let mut packages = package_rows(&global, PiSettingsScope::Global);
        let mut resources = resource_rows(&global, PiSettingsScope::Global);
        if scope == PiSettingsScope::Project {
            packages.extend(package_rows(&project, PiSettingsScope::Project));
            resources.extend(resource_rows(&project, PiSettingsScope::Project));
        }

        let settings = common_settings(&effective);
        let effective_settings_json =
            serde_json::to_string_pretty(&effective).unwrap_or_else(|_| "{}".into());

        Ok(PiSettingsSnapshot {
            runtime: PiRuntimeInfo {
                installed: executable.is_some(),
                version,
                executable: executable.map(|path| path.to_string_lossy().to_string()),
                config_dir: config_dir.to_string_lossy().to_string(),
            },
            scope,
            project_path: project_root.map(|p| p.to_string_lossy().to_string()),
            settings,
            providers,
            openai_compatible,
            packages,
            resources,
            global_settings_path: global_settings_path.to_string_lossy().to_string(),
            project_settings_path: project_settings_path.map(|p| p.to_string_lossy().to_string()),
            auth_path: auth_path.to_string_lossy().to_string(),
            models_path: models_path.to_string_lossy().to_string(),
            effective_settings_json,
        })
    }

    pub async fn set_setting(
        &self,
        scope: PiSettingsScope,
        project_path: Option<&str>,
        key: &str,
        value: Value,
    ) -> Result<PiSettingsSnapshot, String> {
        let path = self.settings_path(scope, project_path)?;
        let mut root = read_object(&path);
        let segments: &[&str] = match key {
            "compaction.enabled" => &["compaction", "enabled"],
            "retry.enabled" => &["retry", "enabled"],
            "defaultProjectTrust" => &["defaultProjectTrust"],
            "transport" => &["transport"],
            _ => return Err(format!("unsupported Pi setting: {key}")),
        };
        set_nested(&mut root, segments, value);
        write_object_atomic(&path, &root, false)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        self.snapshot(scope, project_path).await
    }

    pub async fn set_api_key(
        &self,
        provider: &str,
        key: &str,
        project_path: Option<&str>,
        scope: PiSettingsScope,
    ) -> Result<PiSettingsSnapshot, String> {
        if provider.trim().is_empty() || key.trim().is_empty() {
            return Err("Provider and API key are required".into());
        }
        let path = self.config_dir().join("auth.json");
        let mut auth = read_object(&path);
        auth.insert(
            provider.to_string(),
            serde_json::json!({ "type": "api_key", "key": key.trim() }),
        );
        write_object_atomic(&path, &auth, true)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        self.snapshot(scope, project_path).await
    }

    pub async fn remove_credential(
        &self,
        provider: &str,
        project_path: Option<&str>,
        scope: PiSettingsScope,
    ) -> Result<PiSettingsSnapshot, String> {
        let path = self.config_dir().join("auth.json");
        let mut auth = read_object(&path);
        auth.remove(provider);
        write_object_atomic(&path, &auth, true)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        self.snapshot(scope, project_path).await
    }

    pub async fn set_openai_compatible(
        &self,
        base_url: &str,
        api_key: Option<&str>,
        project_path: Option<&str>,
        scope: PiSettingsScope,
    ) -> Result<PiSettingsSnapshot, String> {
        let base_url = base_url.trim().trim_end_matches('/');
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err("OpenAI-compatible base URL must start with http:// or https://".into());
        }
        let auth_path = self.config_dir().join("auth.json");
        let mut auth = read_object(&auth_path);
        let supplied_key = api_key.map(str::trim).filter(|key| !key.is_empty());
        let stored_key = auth
            .get(OPENAI_COMPATIBLE_PROVIDER)
            .and_then(|entry| entry.get("key"))
            .and_then(Value::as_str);
        let key = supplied_key.or(stored_key);
        let models = discover_openai_models(base_url, key).await?;

        let path = self.config_dir().join("models.json");
        let mut root = read_object(&path);
        let providers = root
            .entry("providers")
            .or_insert_with(|| Value::Object(Map::new()));
        if !providers.is_object() {
            *providers = Value::Object(Map::new());
        }
        providers
            .as_object_mut()
            .expect("providers is an object")
            .insert(
                OPENAI_COMPATIBLE_PROVIDER.into(),
                openai_provider_config(base_url, &models, key.is_some()),
            );
        write_object_atomic(&path, &root, false)
            .map_err(|e| format!("write {}: {e}", path.display()))?;

        if let Some(key) = supplied_key {
            auth.insert(
                OPENAI_COMPATIBLE_PROVIDER.into(),
                serde_json::json!({ "type": "api_key", "key": key }),
            );
            write_object_atomic(&auth_path, &auth, true)
                .map_err(|e| format!("write {}: {e}", auth_path.display()))?;
        }

        self.snapshot(scope, project_path).await
    }

    /// Refresh the configured OpenAI-compatible registry before Pi enumerates
    /// models. A missing custom provider is a no-op; callers may retain the
    /// last successful registry if an already-configured endpoint is offline.
    pub async fn refresh_openai_compatible(&self) -> Result<(), String> {
        let path = self.config_dir().join("models.json");
        let mut root = read_object(&path);
        let Some(provider) = root
            .get_mut("providers")
            .and_then(Value::as_object_mut)
            .and_then(|providers| providers.get_mut(OPENAI_COMPATIBLE_PROVIDER))
            .and_then(Value::as_object_mut)
        else {
            return Ok(());
        };
        let Some(base_url) = provider
            .get("baseUrl")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Err("OpenAI-compatible provider is missing its base URL".into());
        };
        let auth = read_object(&self.config_dir().join("auth.json"));
        let key = auth
            .get(OPENAI_COMPATIBLE_PROVIDER)
            .and_then(|entry| entry.get("key"))
            .and_then(Value::as_str);
        let models = discover_openai_models(&base_url, key).await?;
        provider.insert(
            "models".into(),
            Value::Array(
                models
                    .into_iter()
                    .map(|id| serde_json::json!({ "id": id }))
                    .collect(),
            ),
        );
        write_object_atomic(&path, &root, false)
            .map_err(|error| format!("write {}: {error}", path.display()))
    }

    pub async fn package_action(
        &self,
        action: &str,
        source: &str,
        scope: PiSettingsScope,
        project_path: Option<&str>,
    ) -> Result<PiSettingsSnapshot, String> {
        let executable = nova_harness::pi::resolve_pi_executable()
            .ok_or_else(|| "Pi is not installed on this device".to_string())?;
        if source.trim().is_empty() {
            return Err("Package source is required".into());
        }
        let project_root = validate_project_path(scope, project_path)?;
        let mut command = Command::new(executable);
        match action {
            "install" => {
                command.arg("install").arg(source.trim());
                if scope == PiSettingsScope::Project {
                    command.arg("-l");
                }
            }
            "remove" => {
                command.arg("remove").arg(source.trim());
                if scope == PiSettingsScope::Project {
                    command.arg("-l");
                }
            }
            "update" => {
                command.arg("update").arg(source.trim());
            }
            _ => return Err(format!("unsupported package action: {action}")),
        }
        if let Some(root) = project_root.as_ref() {
            command.current_dir(root);
        }
        command.stdin(Stdio::null());
        let output = command
            .output()
            .await
            .map_err(|e| format!("start Pi package command: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Err(if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("Pi package command exited with {}", output.status)
            });
        }
        self.snapshot(scope, project_path).await
    }

    fn settings_path(
        &self,
        scope: PiSettingsScope,
        project_path: Option<&str>,
    ) -> Result<PathBuf, String> {
        match scope {
            PiSettingsScope::Global => Ok(self.config_dir().join("settings.json")),
            PiSettingsScope::Project => Ok(validate_project_path(scope, project_path)?
                .expect("project scope validated")
                .join(".pi")
                .join("settings.json")),
        }
    }
}

async fn discover_openai_models(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|error| format!("create model registry client: {error}"))?;
    let mut request = client.get(format!("{base_url}/models"));
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("fetch {base_url}/models: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("read {base_url}/models: {error}"))?;
    if !status.is_success() {
        let detail = body.trim().chars().take(240).collect::<String>();
        return Err(if detail.is_empty() {
            format!("{base_url}/models returned {status}")
        } else {
            format!("{base_url}/models returned {status}: {detail}")
        });
    }
    parse_openai_model_registry(&body)
}

fn parse_openai_model_registry(body: &str) -> Result<Vec<String>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| format!("model registry returned invalid JSON: {error}"))?;
    let rows = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .ok_or_else(|| "model registry response is missing a data array".to_string())?;
    let models: Vec<String> = rows
        .iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if models.is_empty() {
        return Err("model registry returned no model IDs".into());
    }
    Ok(models)
}

fn openai_provider_config(base_url: &str, models: &[String], authenticated: bool) -> Value {
    let mut provider = Map::new();
    provider.insert("baseUrl".into(), Value::String(base_url.into()));
    provider.insert("api".into(), Value::String("openai-completions".into()));
    provider.insert(
        "models".into(),
        Value::Array(
            models
                .iter()
                .map(|id| serde_json::json!({ "id": id }))
                .collect(),
        ),
    );
    // Pi requires an auth source before custom models are selectable. A
    // successful unauthenticated registry probe identifies a keyless local
    // endpoint, so give Pi the documented placeholder credential inline.
    if !authenticated {
        provider.insert("apiKey".into(), Value::String("none".into()));
    }
    Value::Object(provider)
}

fn validate_project_path(
    scope: PiSettingsScope,
    project_path: Option<&str>,
) -> Result<Option<PathBuf>, String> {
    if scope == PiSettingsScope::Global {
        return Ok(project_path
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from));
    }
    let path = project_path
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "Project scope requires a project path".to_string())?;
    if !path.is_absolute() {
        return Err("Project path must be absolute".into());
    }
    Ok(Some(path))
}

fn common_settings(root: &Value) -> PiCommonSettings {
    PiCommonSettings {
        default_provider: string_at(root, &["defaultProvider"]),
        default_model: string_at(root, &["defaultModel"]),
        default_thinking_level: string_at(root, &["defaultThinkingLevel"]),
        auto_compaction: bool_at(root, &["compaction", "enabled"]).unwrap_or(true),
        auto_retry: bool_at(root, &["retry", "enabled"]).unwrap_or(true),
        default_project_trust: string_at(root, &["defaultProjectTrust"])
            .unwrap_or_else(|| "ask".into()),
        transport: string_at(root, &["transport"]).unwrap_or_else(|| "auto".into()),
    }
}

fn provider_rows(auth: &Map<String, Value>, models: &Map<String, Value>) -> Vec<PiProviderStatus> {
    let custom: BTreeSet<String> = models
        .get("providers")
        .and_then(Value::as_object)
        .map(|providers| providers.keys().cloned().collect())
        .unwrap_or_default();
    let mut known: BTreeMap<String, (String, String)> = PROVIDERS
        .iter()
        .map(|(id, name, env)| ((*id).into(), ((*name).into(), (*env).into())))
        .collect();
    for id in auth.keys().chain(custom.iter()) {
        known
            .entry(id.clone())
            .or_insert_with(|| (humanize(id), String::new()));
    }
    let mut rows: Vec<_> = known
        .into_iter()
        .map(|(id, (name, env))| {
            let stored = auth.get(&id);
            let env_present =
                !env.is_empty() && std::env::var_os(&env).is_some_and(|v| !v.is_empty());
            let is_custom = custom.contains(&id);
            let (configured, source) = if let Some(entry) = stored {
                let kind = entry.get("type").and_then(Value::as_str).unwrap_or("oauth");
                (
                    true,
                    if kind == "api_key" {
                        "Stored API key"
                    } else {
                        "Stored OAuth"
                    }
                    .to_string(),
                )
            } else if env_present {
                (true, format!("Environment · {env}"))
            } else if is_custom {
                (true, "Custom provider".into())
            } else {
                (false, "Not configured".into())
            };
            PiProviderStatus {
                id,
                name,
                configured,
                source,
                environment_variable: (!env.is_empty()).then_some(env),
                custom: is_custom,
            }
        })
        .collect();
    rows.sort_by_key(|row| (!row.configured, row.name.to_lowercase()));
    rows
}

fn openai_compatible_status(
    auth: &Map<String, Value>,
    root: &Map<String, Value>,
) -> PiOpenAiCompatibleStatus {
    let provider = root
        .get("providers")
        .and_then(Value::as_object)
        .and_then(|providers| providers.get(OPENAI_COMPATIBLE_PROVIDER))
        .and_then(Value::as_object);
    let base_url = provider
        .and_then(|provider| provider.get("baseUrl"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let models = provider
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    PiOpenAiCompatibleStatus {
        configured: provider.is_some(),
        base_url,
        models,
        has_stored_key: auth.contains_key(OPENAI_COMPATIBLE_PROVIDER),
    }
}

fn package_rows(root: &Map<String, Value>, scope: PiSettingsScope) -> Vec<PiPackageInfo> {
    root.get("packages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| match entry {
            Value::String(source) => Some(source.clone()),
            Value::Object(object) => object.get("source")?.as_str().map(str::to_string),
            _ => None,
        })
        .map(|source| {
            let kind = if source.starts_with("npm:") {
                "npm"
            } else if source.starts_with("git:")
                || source.starts_with("http://")
                || source.starts_with("https://")
                || source.starts_with("ssh://")
            {
                "git"
            } else {
                "local"
            };
            let pinned = match kind {
                "npm" => {
                    let spec = source.trim_start_matches("npm:");
                    if spec.starts_with('@') {
                        spec.find('/')
                            .is_some_and(|slash| spec[slash + 1..].contains('@'))
                    } else {
                        spec.contains('@')
                    }
                }
                "git" => source.rsplit_once('@').is_some(),
                _ => false,
            };
            PiPackageInfo {
                source,
                scope,
                kind: kind.into(),
                pinned,
            }
        })
        .collect()
}

fn resource_rows(root: &Map<String, Value>, scope: PiSettingsScope) -> Vec<PiResourceInfo> {
    ["extensions", "skills", "prompts", "themes"]
        .into_iter()
        .flat_map(|kind| {
            root.get(kind)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(move |path| PiResourceInfo {
                    kind: kind.into(),
                    path: path.into(),
                    scope,
                })
        })
        .collect()
}

fn read_object(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn write_object_atomic(path: &Path, object: &Map<String, Value>, secret: bool) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(&Value::Object(object.clone()))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

fn set_nested(root: &mut Map<String, Value>, segments: &[&str], value: Value) {
    if segments.len() == 1 {
        root.insert(segments[0].to_string(), value);
        return;
    }
    let child = root
        .entry(segments[0].to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !child.is_object() {
        *child = Value::Object(Map::new());
    }
    set_nested(
        child.as_object_mut().expect("object above"),
        &segments[1..],
        value,
    );
}

fn deep_merge(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Object(mut base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                let merged = base
                    .remove(&key)
                    .map_or(value.clone(), |old| deep_merge(old, value));
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (_, overlay) => overlay,
    }
}

fn string_at(root: &Value, path: &[&str]) -> Option<String> {
    path.iter()
        .try_fold(root, |value, key| value.get(*key))?
        .as_str()
        .map(str::to_string)
}

fn bool_at(root: &Value, path: &[&str]) -> Option<bool> {
    path.iter()
        .try_fold(root, |value, key| value.get(*key))?
        .as_bool()
}

fn humanize(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn project_settings_merge_nested_objects() {
        let global = serde_json::json!({"compaction": {"enabled": true, "reserveTokens": 10}});
        let project = serde_json::json!({"compaction": {"enabled": false}});
        assert_eq!(
            deep_merge(global, project),
            serde_json::json!({"compaction": {"enabled": false, "reserveTokens": 10}})
        );
    }

    #[test]
    fn package_projection_preserves_scope_and_pin() {
        let root = serde_json::json!({"packages": ["npm:@acme/tools@1.2.0", {"source": "git:github.com/acme/pi@v1"}]});
        let rows = package_rows(root.as_object().unwrap(), PiSettingsScope::Global);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.pinned));
    }

    #[test]
    fn openai_compatible_projection_never_exposes_the_key() {
        let auth = serde_json::json!({
            "openai-compatible": {"type": "api_key", "key": "secret"}
        });
        let models = serde_json::json!({
            "providers": {
                "openai-compatible": {
                    "baseUrl": "http://localhost:1234/v1",
                    "api": "openai-completions",
                    "models": [{"id": "local-model"}]
                }
            }
        });
        let status =
            openai_compatible_status(auth.as_object().unwrap(), models.as_object().unwrap());
        assert!(status.configured);
        assert!(status.has_stored_key);
        assert_eq!(status.base_url.as_deref(), Some("http://localhost:1234/v1"));
        assert_eq!(status.models, ["local-model"]);
    }

    #[test]
    fn openai_model_registry_extracts_and_sorts_ids() {
        assert_eq!(
            parse_openai_model_registry(
                r#"{"object":"list","data":[{"id":"z-model"},{"id":"a-model"},{"id":"a-model"}]}"#,
            )
            .unwrap(),
            ["a-model", "z-model"]
        );
        assert!(parse_openai_model_registry(r#"{"data":[]}"#).is_err());
    }

    #[test]
    fn keyless_openai_provider_gets_pi_placeholder_auth() {
        let models = vec!["local-model".to_string()];
        let keyless = openai_provider_config("http://localhost:1234/v1", &models, false);
        assert_eq!(keyless["apiKey"], "none");
        assert_eq!(keyless["models"][0]["id"], "local-model");
        let authenticated = openai_provider_config("https://example.com/v1", &models, true);
        assert!(authenticated.get("apiKey").is_none());
    }

    #[tokio::test]
    async fn openai_model_registry_is_fetched_with_bearer_auth() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /v1/models HTTP/1.1"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-key")
            );
            let body = r#"{"data":[{"id":"local-b"},{"id":"local-a"}]}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });

        let models = discover_openai_models(&format!("http://{address}/v1"), Some("test-key"))
            .await
            .unwrap();
        assert_eq!(models, ["local-a", "local-b"]);
        server.await.unwrap();
    }
}
