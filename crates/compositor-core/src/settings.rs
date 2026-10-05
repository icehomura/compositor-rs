//! `ToolDefaults`: toggles that belong to the person rather than to a document — Auto Select, the
//! transform box, rulers, guides, the grid (and its spacing and look) and the snapping switches. They
//! keep whatever they were last set to, across tabs and across launches.
//!
//! Tests get the compiled defaults instead, so one test flipping a switch cannot reach another — or the
//! app the person is actually using.

use parking_lot::RwLock;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::LazyLock;

static STORE: LazyLock<RwLock<Map<String, Value>>> = LazyLock::new(|| RwLock::new(load()));
static TESTING: LazyLock<RwLock<bool>> = LazyLock::new(|| RwLock::new(false));

/// Tests call this so the process keeps the compiled defaults instead of the person's file.
pub fn set_testing(value: bool) {
    *TESTING.write() = value;
}

fn is_testing() -> bool {
    *TESTING.read()
}

fn config_directory() -> PathBuf {
    if let Ok(path) = std::env::var("COMPOSITOR_CONFIG_DIR") {
        return PathBuf::from(path);
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(app_data) = std::env::var("APPDATA") {
            return PathBuf::from(app_data).join("Compositor");
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".config").join("compositor");
        }
    }
    PathBuf::from(".")
}

fn defaults_path() -> PathBuf {
    config_directory().join("tool-defaults.json")
}

fn load() -> Map<String, Value> {
    std::fs::read_to_string(defaults_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// Writes the store out; failures are silently ignored, as `UserDefaults` failures are.
pub fn persist() {
    if is_testing() {
        return;
    }
    let snapshot = STORE.read().clone();
    let directory = config_directory();
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(&Value::Object(snapshot)) {
        let _ = std::fs::write(defaults_path(), text);
    }
}

/// The stored key, prefixed the way `ToolDefaults` prefixed it.
fn key_of(key: &str) -> String {
    format!("tool.{key}")
}

pub fn bool_value(key: &str, fallback: bool) -> bool {
    if is_testing() {
        return fallback;
    }
    STORE
        .read()
        .get(&key_of(key))
        .and_then(|value| value.as_bool())
        .unwrap_or(fallback)
}

pub fn set_bool(value: bool, key: &str) {
    if is_testing() {
        return;
    }
    STORE.write().insert(key_of(key), Value::Bool(value));
    persist();
}

pub fn int_value(key: &str, fallback: i64) -> i64 {
    if is_testing() {
        return fallback;
    }
    STORE
        .read()
        .get(&key_of(key))
        .and_then(|value| value.as_i64())
        .unwrap_or(fallback)
}

pub fn set_int(value: i64, key: &str) {
    if is_testing() {
        return;
    }
    STORE.write().insert(key_of(key), Value::from(value));
    persist();
}

pub fn string_value(key: &str, fallback: &str) -> String {
    if is_testing() {
        return fallback.to_string();
    }
    STORE
        .read()
        .get(&key_of(key))
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

pub fn set_string(value: &str, key: &str) {
    if is_testing() {
        return;
    }
    STORE.write().insert(key_of(key), Value::from(value));
    persist();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_get_the_compiled_defaults() {
        set_testing(true);
        set_bool(true, "autoSelect");
        assert!(!bool_value("autoSelect", false));
        set_testing(false);
    }
}
