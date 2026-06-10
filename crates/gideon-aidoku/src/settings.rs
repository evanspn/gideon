//! Minimal settings types vendored from bobo-koreader's `settings/schema.rs`.
//!
//! Only the types that the source runtime actually needs are kept here; the
//! rest of bobo's settings module (storage limits, library view modes, user
//! profiles, ...) is intentionally not vendored.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A single source-specific setting value, as stored by the `defaults` WASM
/// imports and the source settings storage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SourceSettingValue {
    Data(Vec<u8>),
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Vec(Vec<String>),
    Null,
}

/// Minimal stand-in for bobo's global `Settings` struct. The source runtime
/// only reads `languages` (exposed to sources through the `defaults` imports)
/// and `source_settings` (the per-source stored settings).
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Settings {
    /// If set, only chapters translated to those languages will be shown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,

    /// Source-specific settings.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub source_settings: HashMap<String, HashMap<String, SourceSettingValue>>,
}
