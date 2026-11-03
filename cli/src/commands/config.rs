//! `oak config` and the agent-sync settings it manages.
//!
//! Three boolean keys control `oak watch` (see `commands::watch`):
//!
//! - `watch.enabled`   — run the watch daemon at all (`oak start`, the
//!   SessionStart hook)
//! - `watch.auto_pull` — merge main into the branch when it moves
//! - `watch.auto_push` — publish every new local commit
//!
//! Each key resolves through five layers; the first that sets it wins:
//!
//! 1. the environment (`OAK_WATCH`, `OAK_WATCH_AUTO_PULL`,
//!    `OAK_WATCH_AUTO_PUSH`; `1/true/yes/on` or `0/false/no/off`)
//! 2. the checkout's repo config (`MetadataKey::WatchConfig`)
//! 3. the global config, `~/.oak/config.json`
//! 4. the signed-in user's oak.space preference
//!    (`GET /api/user/preferences` → `agent_sync`), read from a local cache
//!    the watch daemon refreshes — resolution itself never touches the
//!    network, so hooks stay fast
//! 5. the default, `true`
//!
//! Both config files use the same JSON shape,
//! `{"watch": {"enabled": false}}`; unknown fields are preserved on write.

use std::path::{Path, PathBuf};
use std::time::Duration;

use oak_core::{MetadataKey, OakError, Repository, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::output;

/// A known `oak config` key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigKey {
    WatchEnabled,
    WatchAutoPull,
    WatchAutoPush,
}

impl ConfigKey {
    pub const ALL: [ConfigKey; 3] = [
        ConfigKey::WatchEnabled,
        ConfigKey::WatchAutoPull,
        ConfigKey::WatchAutoPush,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ConfigKey::WatchEnabled => "watch.enabled",
            ConfigKey::WatchAutoPull => "watch.auto_pull",
            ConfigKey::WatchAutoPush => "watch.auto_push",
        }
    }

    /// Field inside the config files' `watch` object.
    fn field(self) -> &'static str {
        match self {
            ConfigKey::WatchEnabled => "enabled",
            ConfigKey::WatchAutoPull => "auto_pull",
            ConfigKey::WatchAutoPush => "auto_push",
        }
    }

    pub fn env_var(self) -> &'static str {
        match self {
            ConfigKey::WatchEnabled => "OAK_WATCH",
            ConfigKey::WatchAutoPull => "OAK_WATCH_AUTO_PULL",
            ConfigKey::WatchAutoPush => "OAK_WATCH_AUTO_PUSH",
        }
    }

    /// Field of the server's `agent_sync` preference object.
    fn server_field(self) -> &'static str {
        match self {
            ConfigKey::WatchEnabled => "watch",
            ConfigKey::WatchAutoPull => "auto_pull",
            ConfigKey::WatchAutoPush => "auto_push",
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|key| key.name() == name)
            .ok_or_else(|| {
                OakError::InvalidArgument(format!(
                    "unknown config key '{name}'; known keys: {}",
                    Self::ALL.map(|key| key.name()).join(", ")
                ))
            })
    }
}

/// Which layer decided a setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Env,
    Repo,
    Global,
    Server,
    Default,
}

impl Source {
    pub fn describe(self) -> &'static str {
        match self {
            Source::Env => "environment",
            Source::Repo => "repo config",
            Source::Global => "global config (~/.oak/config.json)",
            Source::Server => "oak.space preference",
            Source::Default => "default",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Setting {
    pub value: bool,
    pub source: Source,
}

/// Effective agent-sync settings for one checkout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct AgentSyncSettings {
    pub enabled: Setting,
    pub auto_pull: Setting,
    pub auto_push: Setting,
}

impl AgentSyncSettings {
    pub fn get(&self, key: ConfigKey) -> Setting {
        match key {
            ConfigKey::WatchEnabled => self.enabled,
            ConfigKey::WatchAutoPull => self.auto_pull,
            ConfigKey::WatchAutoPush => self.auto_push,
        }
    }
}

/// The layers a resolution reads, loaded once so a resolution is cheap and
/// consistent.
#[derive(Default)]
pub struct Layers {
    pub repo: Value,
    pub global: Value,
    pub server: Option<ServerPreferences>,
}

impl Layers {
    /// Load every local layer for `repo` (optional: outside a repository
    /// only the global and server layers apply). `remote` selects the cached
    /// server preference.
    pub fn load(repo: Option<&dyn Repository>, remote: Option<&str>) -> Self {
        Self {
            repo: repo.map(load_repo_layer).unwrap_or(Value::Null),
            global: load_global_layer(),
            server: remote.and_then(cached_server_preferences),
        }
    }

    pub fn resolve(&self) -> AgentSyncSettings {
        AgentSyncSettings {
            enabled: self.resolve_key(ConfigKey::WatchEnabled, env_bool),
            auto_pull: self.resolve_key(ConfigKey::WatchAutoPull, env_bool),
            auto_push: self.resolve_key(ConfigKey::WatchAutoPush, env_bool),
        }
    }

    fn resolve_key(&self, key: ConfigKey, env: impl Fn(&str) -> Option<bool>) -> Setting {
        let layered = [
            (env(key.env_var()), Source::Env),
            (layer_value(&self.repo, key), Source::Repo),
            (layer_value(&self.global, key), Source::Global),
            (
                self.server.as_ref().and_then(|prefs| prefs.get(key)),
                Source::Server,
            ),
        ];
        layered
            .into_iter()
            .find_map(|(value, source)| value.map(|value| Setting { value, source }))
            .unwrap_or(Setting {
                value: true,
                source: Source::Default,
            })
    }
}

/// Resolve the effective settings for the checkout containing `path`
/// (global/server/default only when `path` is not inside a repository).
/// Local reads only.
pub fn resolve_for_path(path: &Path) -> AgentSyncSettings {
    let repo = crate::resolve::resolve(path)
        .ok()
        .and_then(|ctx| ctx.open().ok());
    let remote = repo.as_deref().and_then(|repo| {
        repo.get_metadata(MetadataKey::RemoteUrl)
            .ok()
            .flatten()
            .and_then(|url| super::push::normalize_remote_url(&url))
    });
    Layers::load(repo.as_deref(), remote.as_deref()).resolve()
}

/// Parse a boolean config value (`1/true/yes/on`, `0/false/no/off`).
pub fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().as_deref().and_then(parse_bool)
}

fn layer_value(layer: &Value, key: ConfigKey) -> Option<bool> {
    layer.get("watch")?.get(key.field())?.as_bool()
}

fn set_layer_value(layer: &mut Value, key: ConfigKey, value: Option<bool>) {
    if !layer.is_object() {
        *layer = Value::Object(Map::new());
    }
    let root = layer.as_object_mut().expect("layer is an object");
    let watch = root
        .entry("watch")
        .or_insert_with(|| Value::Object(Map::new()));
    if !watch.is_object() {
        *watch = Value::Object(Map::new());
    }
    let watch_map = watch.as_object_mut().expect("watch is an object");
    match value {
        Some(value) => {
            watch_map.insert(key.field().to_string(), Value::Bool(value));
        }
        None => {
            watch_map.remove(key.field());
        }
    }
    if watch_map.is_empty() {
        root.remove("watch");
    }
}

// ---------------------------------------------------------------------------
// Layers on disk
// ---------------------------------------------------------------------------

pub fn global_config_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".oak").join("config.json"))
}

fn load_global_layer() -> Value {
    global_config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

fn load_repo_layer(repo: &dyn Repository) -> Value {
    repo.get_metadata(MetadataKey::WatchConfig)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

fn write_global_layer(layer: &Value) -> Result<PathBuf> {
    let path = global_config_path().ok_or_else(|| {
        OakError::InvalidArgument("could not determine your home directory".to_string())
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&non_null(layer))
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    crate::atomic_file::write_atomic(&path, format!("{text}\n"))?;
    Ok(path)
}

fn write_repo_layer(repo: &dyn Repository, layer: &Value) -> Result<()> {
    let text = serde_json::to_string(&non_null(layer))
        .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
    repo.set_metadata(MetadataKey::WatchConfig, &text)
}

fn non_null(layer: &Value) -> Value {
    if layer.is_null() {
        Value::Object(Map::new())
    } else {
        layer.clone()
    }
}

// ---------------------------------------------------------------------------
// Server preference (GET /api/user/preferences) and its local cache
// ---------------------------------------------------------------------------

/// The server's `agent_sync` preference object. Every field optional: an
/// absent field leaves the key to the default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerPreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_pull: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_push: Option<bool>,
}

impl ServerPreferences {
    fn get(&self, key: ConfigKey) -> Option<bool> {
        match key.server_field() {
            "watch" => self.watch,
            "auto_pull" => self.auto_pull,
            _ => self.auto_push,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct PreferenceCache {
    #[serde(default)]
    servers: std::collections::BTreeMap<String, CachedPreference>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedPreference {
    fetched_at: u64,
    agent_sync: ServerPreferences,
}

fn preference_cache_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".oak")
            .join("agent-sync-preferences.json"),
    )
}

fn load_preference_cache() -> PreferenceCache {
    preference_cache_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The last server preference the watch daemon fetched for `remote`.
pub fn cached_server_preferences(remote: &str) -> Option<ServerPreferences> {
    load_preference_cache()
        .servers
        .get(remote)
        .map(|cached| cached.agent_sync.clone())
}

/// When the cached preference for `remote` was fetched (unix seconds).
pub fn cached_server_preferences_at(remote: &str) -> Option<u64> {
    load_preference_cache()
        .servers
        .get(remote)
        .map(|cached| cached.fetched_at)
}

fn store_cached_preferences(remote: &str, prefs: Option<&ServerPreferences>) {
    let Some(path) = preference_cache_path() else {
        return;
    };
    let mut cache = load_preference_cache();
    match prefs {
        Some(prefs) => {
            cache.servers.insert(
                remote.to_string(),
                CachedPreference {
                    fetched_at: unix_now(),
                    agent_sync: prefs.clone(),
                },
            );
        }
        None => {
            cache.servers.remove(remote);
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&cache) {
        let _ = crate::atomic_file::write_atomic(&path, text);
    }
}

#[derive(Deserialize)]
struct PreferencesResponse {
    #[serde(default)]
    agent_sync: Option<ServerPreferences>,
}

/// Fetch the signed-in user's agent-sync preference from `remote` and
/// refresh the local cache. `Ok(None)` when the server has no such endpoint
/// (404: older oak.space, `oak serve`) or there is no signed-in user (401):
/// the cache entry is dropped so a stale preference cannot linger.
pub async fn fetch_server_preferences(
    remote: &str,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<Option<ServerPreferences>> {
    let Some(api_key) = api_key else {
        store_cached_preferences(remote, None);
        return Ok(None);
    };
    let response = crate::http::api_client()
        .get(format!(
            "{}/api/user/preferences",
            remote.trim_end_matches('/')
        ))
        .bearer_auth(api_key)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND
        || status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        store_cached_preferences(remote, None);
        return Ok(None);
    }
    if !status.is_success() {
        return Err(crate::http::server_error(response).await);
    }
    let body: PreferencesResponse = response
        .json()
        .await
        .map_err(|e| OakError::Http(e.to_string()))?;
    let prefs = body.agent_sync.unwrap_or_default();
    store_cached_preferences(remote, Some(&prefs));
    Ok(Some(prefs))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// `oak config get|set|unset|list`
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct KeyReport {
    key: &'static str,
    value: bool,
    source: Source,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    global: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<bool>,
}

#[derive(Serialize)]
struct ListReport {
    schema_version: u32,
    kind: &'static str,
    /// `effective` (every layer) or `global` (only `~/.oak/config.json`).
    scope: &'static str,
    keys: Vec<KeyReport>,
    global_config: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_root: Option<String>,
    precedence: [&'static str; 5],
}

#[derive(Serialize)]
struct ChangeReport {
    schema_version: u32,
    kind: &'static str,
    key: &'static str,
    /// `null` after `unset`.
    value: Option<bool>,
    scope: &'static str,
    effective: Setting,
}

struct ConfigContext {
    repo: Option<Box<dyn Repository>>,
    work_tree: Option<PathBuf>,
    remote: Option<String>,
}

fn open_context(cwd: &Path, require_repo: bool) -> Result<ConfigContext> {
    let ctx =
        match crate::resolve::resolve(cwd) {
            Ok(ctx) => Some(ctx),
            Err(OakError::RepoNotFound) if !require_repo => None,
            Err(OakError::RepoNotFound) => return Err(OakError::InvalidArgument(
                "not inside a repository — run this from a checkout to change its repo config, \
                 or pass --global to change ~/.oak/config.json"
                    .to_string(),
            )),
            Err(e) => return Err(e),
        };
    let (repo, work_tree) = match ctx {
        Some(ctx) => (Some(ctx.open()?), Some(ctx.work_tree)),
        None => (None, None),
    };
    let remote = repo.as_deref().and_then(|repo| {
        repo.get_metadata(MetadataKey::RemoteUrl)
            .ok()
            .flatten()
            .and_then(|url| super::push::normalize_remote_url(&url))
    });
    Ok(ConfigContext {
        repo,
        work_tree,
        remote,
    })
}

fn key_report(layers: &Layers, settings: &AgentSyncSettings, key: ConfigKey) -> KeyReport {
    let setting = settings.get(key);
    KeyReport {
        key: key.name(),
        value: setting.value,
        source: setting.source,
        env: env_bool(key.env_var()),
        repo: layer_value(&layers.repo, key),
        global: layer_value(&layers.global, key),
        server: layers.server.as_ref().and_then(|prefs| prefs.get(key)),
    }
}

/// `oak config get KEY [--global]`.
pub fn get(cwd: &Path, key: &str, global: bool, json: bool) -> Result<()> {
    let key = ConfigKey::parse(key)?;
    if global {
        let value = layer_value(&load_global_layer(), key);
        if json {
            return output::print_json(&serde_json::json!({
                "schema_version": 1,
                "kind": "config_value",
                "key": key.name(),
                "scope": "global",
                "value": value,
            }));
        }
        output::print_line(&value.map_or("(unset)".to_string(), |v| v.to_string()));
        return Ok(());
    }
    let ctx = open_context(cwd, false)?;
    let layers = Layers::load(ctx.repo.as_deref(), ctx.remote.as_deref());
    let settings = layers.resolve();
    if json {
        return output::print_json(&key_report(&layers, &settings, key));
    }
    output::print_line(&settings.get(key).value.to_string());
    Ok(())
}

/// `oak config set KEY VALUE [--global]` and `oak config unset KEY
/// [--global]` (`value: None`).
pub fn set(cwd: &Path, key: &str, value: Option<&str>, global: bool, json: bool) -> Result<()> {
    let key = ConfigKey::parse(key)?;
    let value = value
        .map(|raw| {
            parse_bool(raw).ok_or_else(|| {
                OakError::InvalidArgument(format!(
                    "{} takes a boolean (true/false, on/off, 1/0), not '{raw}'",
                    key.name()
                ))
            })
        })
        .transpose()?;
    let ctx = open_context(cwd, !global)?;
    let scope;
    let mut where_ = String::new();
    if global {
        let mut layer = load_global_layer();
        set_layer_value(&mut layer, key, value);
        let path = write_global_layer(&layer)?;
        scope = "global";
        where_ = path.display().to_string();
    } else {
        let repo = ctx
            .repo
            .as_deref()
            .expect("open_context requires a repository without --global");
        let mut layer = load_repo_layer(repo);
        set_layer_value(&mut layer, key, value);
        write_repo_layer(repo, &layer)?;
        scope = "repo";
        if let Some(root) = &ctx.work_tree {
            where_ = format!("this checkout ({})", root.display());
        }
    }
    let effective = Layers::load(ctx.repo.as_deref(), ctx.remote.as_deref())
        .resolve()
        .get(key);
    if json {
        return output::print_json(&ChangeReport {
            schema_version: 1,
            kind: "config_change",
            key: key.name(),
            value,
            scope,
            effective,
        });
    }
    match value {
        Some(value) => output::success(&format!("Set {} = {value} in {where_}", key.name())),
        None => output::success(&format!("Unset {} in {where_}", key.name())),
    }
    if effective.source == Source::Env && value.is_some_and(|value| value != effective.value) {
        output::warning(&format!(
            "{} is overridden by {} = {}",
            key.name(),
            key.env_var(),
            effective.value
        ));
    }
    Ok(())
}

/// `oak config list [--global]`.
pub fn list(cwd: &Path, global: bool, json: bool) -> Result<()> {
    let ctx = open_context(cwd, false)?;
    let layers = if global {
        Layers {
            global: load_global_layer(),
            ..Layers::default()
        }
    } else {
        Layers::load(ctx.repo.as_deref(), ctx.remote.as_deref())
    };
    let settings = layers.resolve();
    let report = ListReport {
        schema_version: 1,
        kind: "config_list",
        scope: if global { "global" } else { "effective" },
        keys: ConfigKey::ALL
            .into_iter()
            .map(|key| key_report(&layers, &settings, key))
            .collect(),
        global_config: global_config_path().map(|path| path.display().to_string()),
        repository_root: ctx
            .work_tree
            .as_ref()
            .map(|root| root.display().to_string()),
        precedence: ["env", "repo", "global", "server", "default"],
    };
    if json {
        return output::print_json(&report);
    }
    for key in &report.keys {
        if global {
            let value = key.global.map_or("(unset)".to_string(), |v| v.to_string());
            output::print_line(&format!("{:<16} {value}", key.key));
        } else {
            output::print_line(&format!(
                "{:<16} {:<5}  ({})",
                key.key,
                key.value,
                key.source.describe()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layers(repo: Value, global: Value, server: Option<ServerPreferences>) -> Layers {
        Layers {
            repo,
            global,
            server,
        }
    }

    #[test]
    fn precedence_is_env_repo_global_server_default() {
        let all = layers(
            serde_json::json!({"watch": {"enabled": false}}),
            serde_json::json!({"watch": {"enabled": true, "auto_pull": false}}),
            Some(ServerPreferences {
                watch: Some(true),
                auto_pull: Some(true),
                auto_push: Some(false),
            }),
        );
        let no_env = |_: &str| None;
        assert_eq!(
            all.resolve_key(ConfigKey::WatchEnabled, no_env),
            Setting {
                value: false,
                source: Source::Repo
            }
        );
        assert_eq!(
            all.resolve_key(ConfigKey::WatchAutoPull, no_env),
            Setting {
                value: false,
                source: Source::Global
            }
        );
        assert_eq!(
            all.resolve_key(ConfigKey::WatchAutoPush, no_env),
            Setting {
                value: false,
                source: Source::Server
            }
        );
        let env_on = |_: &str| Some(true);
        assert_eq!(
            all.resolve_key(ConfigKey::WatchEnabled, env_on),
            Setting {
                value: true,
                source: Source::Env
            }
        );
        let empty = layers(Value::Null, Value::Null, None);
        assert_eq!(
            empty.resolve_key(ConfigKey::WatchAutoPush, no_env),
            Setting {
                value: true,
                source: Source::Default
            }
        );
    }

    #[test]
    fn set_and_unset_preserve_unknown_fields() {
        let mut layer = serde_json::json!({"other": 1, "watch": {"future": "x"}});
        set_layer_value(&mut layer, ConfigKey::WatchAutoPush, Some(false));
        assert_eq!(layer_value(&layer, ConfigKey::WatchAutoPush), Some(false));
        assert_eq!(layer["other"], 1);
        assert_eq!(layer["watch"]["future"], "x");
        set_layer_value(&mut layer, ConfigKey::WatchAutoPush, None);
        assert_eq!(layer_value(&layer, ConfigKey::WatchAutoPush), None);
        assert_eq!(layer["watch"]["future"], "x");

        let mut empty = Value::Null;
        set_layer_value(&mut empty, ConfigKey::WatchEnabled, Some(true));
        set_layer_value(&mut empty, ConfigKey::WatchEnabled, None);
        assert_eq!(empty, serde_json::json!({}));
    }

    #[test]
    fn booleans_parse_leniently() {
        for raw in ["1", "true", "YES", " on "] {
            assert_eq!(parse_bool(raw), Some(true), "{raw}");
        }
        for raw in ["0", "false", "No", "off"] {
            assert_eq!(parse_bool(raw), Some(false), "{raw}");
        }
        assert_eq!(parse_bool("maybe"), None);
        assert!(ConfigKey::parse("watch.nope").is_err());
        assert_eq!(
            ConfigKey::parse("watch.auto_pull").unwrap(),
            ConfigKey::WatchAutoPull
        );
    }
}
