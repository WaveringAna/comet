//! Disk cache for the model catalog — harness descriptors + per-harness model
//! lists, keyed by the device that hosts them (`""` = the engine we're
//! connected to). Catalogs are per-DEVICE because the agents run on the
//! project's host, which may be a different machine than the viewer.
//!
//! `Pickers` seeds every Idle catalog slot from this cache (boot, project
//! switch, retry) so the picker paints last-known rows instead of a skeleton,
//! then merges fresh pull results in silently — new models appear and vanished
//! ones drop without a Loading flash. The merge logic itself lives next to
//! `sort_models` in `pickers.rs`; this module only owns persistence.
//!
//! A small JSON file beside `composer-defaults.json`, written atomically
//! (temp file + rename) whenever a pull changed the displayed rows.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use nova_engine::registry::HarnessDescriptor;
use nova_proto::{HarnessId, Model};

const FILE_NAME: &str = "model-catalog-cache.json";

/// The disk-cached model catalog.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelCatalogCache {
    /// Harness descriptors per device id (`""` = the connected engine's own).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub harnesses_by_device: HashMap<String, Vec<HarnessDescriptor>>,
    /// Model lists per `{harness}@{device}` key.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub models_by_key: HashMap<String, Vec<Model>>,
}

impl ModelCatalogCache {
    /// Load from `{data_dir}/model-catalog-cache.json`; defaults on any
    /// failure (missing or corrupt files fall back to an empty cache).
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<Self>(&text) {
                Ok(cache) => cache,
                Err(err) => {
                    tracing::warn!(error = %err, "model-catalog cache corrupt; using empty cache");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Write atomically (temp file + rename) so a crash mid-write never
    /// corrupts the cache.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }

    /// Cache key for a harness's model list: `"{harness}@{device}"`.
    pub fn models_key(harness: HarnessId, device: &str) -> String {
        format!("{}@{device}", harness_key(harness))
    }

    /// Cached harness descriptors for a device, if any.
    pub fn harnesses_for(&self, device: &str) -> Option<Vec<HarnessDescriptor>> {
        self.harnesses_by_device.get(device).cloned()
    }

    /// Cached model list for a harness on a device, if any.
    pub fn models_for(&self, harness: HarnessId, device: &str) -> Option<Vec<Model>> {
        self.models_by_key
            .get(&Self::models_key(harness, device))
            .cloned()
    }

    /// Remember a fresh harness catalog; returns whether the cache changed.
    pub fn store_harnesses(&mut self, device: &str, list: Vec<HarnessDescriptor>) -> bool {
        if self.harnesses_by_device.get(device) == Some(&list) {
            return false;
        }
        self.harnesses_by_device.insert(device.to_string(), list);
        true
    }

    /// Remember a fresh model list; returns whether the cache changed.
    pub fn store_models(&mut self, harness: HarnessId, device: &str, models: Vec<Model>) -> bool {
        let key = Self::models_key(harness, device);
        if self.models_by_key.get(&key) == Some(&models) {
            return false;
        }
        self.models_by_key.insert(key, models);
        true
    }
}

/// Wire-format harness id (matches the proto's kebab-case serialization, so
/// cache keys stay stable if the enum ever grows).
fn harness_key(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::Pi => "pi",
        HarnessId::ClaudeCode => "claude-code",
        HarnessId::Codex => "codex",
        HarnessId::Cursor => "cursor",
        HarnessId::Mock => "mock",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_proto::ReasoningLevel;

    fn model(id: &str, label: &str) -> Model {
        Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: vec![],
            options: vec![],
        }
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ModelCatalogCache::default();
        assert!(cache.store_harnesses(
            "",
            vec![HarnessDescriptor {
                id: HarnessId::Pi,
                name: "Pi".into(),
                supports_steering: true,
                steering_mode: nova_proto::SteeringMode::StepBoundary,
                reasoning_levels: vec![ReasoningLevel::Low, ReasoningLevel::High],
            }],
        ));
        assert!(cache.store_models(HarnessId::Pi, "", vec![model("a", "A")]));
        assert!(!cache.store_models(HarnessId::Pi, "", vec![model("a", "A")]));

        cache.save(dir.path()).unwrap();
        let loaded = ModelCatalogCache::load(dir.path());
        assert_eq!(loaded, cache);
        assert_eq!(loaded.models_for(HarnessId::Pi, "").unwrap()[0].label, "A");
    }

    #[test]
    fn keys_are_per_harness_and_device() {
        let mut cache = ModelCatalogCache::default();
        cache.store_models(HarnessId::Pi, "", vec![model("local", "Local")]);
        cache.store_models(HarnessId::Pi, "dev-2", vec![model("remote", "Remote")]);
        assert_eq!(cache.models_for(HarnessId::Pi, "").unwrap()[0].id, "local");
        assert_eq!(
            cache.models_for(HarnessId::Pi, "dev-2").unwrap()[0].id,
            "remote"
        );
        // Distinct harnesses don't collide under the same device.
        assert!(cache.models_for(HarnessId::ClaudeCode, "").is_none());
    }

    #[test]
    fn missing_and_corrupt_files_yield_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ModelCatalogCache::load(dir.path()),
            ModelCatalogCache::default()
        );
        std::fs::write(ModelCatalogCache::path(dir.path()), "{nope").unwrap();
        assert_eq!(
            ModelCatalogCache::load(dir.path()),
            ModelCatalogCache::default()
        );
    }
}
