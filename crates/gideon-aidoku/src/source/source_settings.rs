use std::{cell::RefCell, collections::HashMap, fs, path::PathBuf};

use anyhow::{Context, Result};

use crate::settings::SourceSettingValue;

use super::model::SettingDefinition;

pub struct SourceSettings {
    source_id: String,
    defaults: HashMap<String, SourceSettingValue>,
    stored: RefCell<HashMap<String, SourceSettingValue>>,
    /// File the stored settings are persisted to on `save`, if any.
    ///
    /// Upstream (bobo-koreader) persisted settings through its `SourceManager`
    /// and database; here we keep a plain JSON file per source instead.
    storage_path: Option<PathBuf>,
}
impl std::fmt::Debug for SourceSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceSettings")
            .field("source_id", &self.source_id)
            .field("defaults", &self.defaults)
            .field("stored", &self.stored)
            .field("storage_path", &self.storage_path)
            .finish()
    }
}

impl SourceSettings {
    pub fn new(
        source_id: String,
        setting_definitions: &[SettingDefinition],
        stored_settings: &HashMap<String, SourceSettingValue>,
        storage_path: Option<PathBuf>,
    ) -> Result<Self> {
        let defaults: HashMap<_, _> = setting_definitions
            .iter()
            .flat_map(default_values_for_definition)
            .collect();

        Ok(Self {
            source_id,
            defaults,
            stored: RefCell::new(stored_settings.clone()),
            storage_path,
        })
    }

    pub fn get(&self, key: &String) -> Option<SourceSettingValue> {
        self.stored
            .borrow()
            .get(key)
            .cloned()
            .or_else(|| self.defaults.get(key).cloned())
    }

    pub fn set(&self, key: &str, value: SourceSettingValue) {
        self.stored.borrow_mut().insert(key.to_owned(), value);
    }

    pub fn save(&self, key: &str, value: SourceSettingValue) -> Result<()> {
        let snapshot = {
            let mut store = self.stored.borrow_mut();
            store.insert(key.to_owned(), value);
            store.clone()
        };

        if let Some(path) = &self.storage_path {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("while creating {}", parent.display()))?;
            }

            fs::write(path, serde_json::to_string(&snapshot)?).with_context(|| {
                format!(
                    "while writing settings for source {} to {}",
                    self.source_id,
                    path.display()
                )
            })?;
        }

        Ok(())
    }
}
// only pub use in aidoku android
pub fn default_values_for_definition(
    setting_definition: &SettingDefinition,
) -> HashMap<String, SourceSettingValue> {
    match setting_definition {
        SettingDefinition::Group { items, .. } => items
            .iter()
            .flat_map(default_values_for_definition)
            .collect(),
        SettingDefinition::Select {
            key,
            default,
            values,
            ..
        } => HashMap::from([(
            key.clone(),
            SourceSettingValue::String(
                default
                    .clone()
                    .unwrap_or_else(|| values.first().cloned().unwrap_or_default()),
            ),
        )]),
        SettingDefinition::MultiSelect { key, default, .. } => {
            HashMap::from([(key.clone(), SourceSettingValue::Vec(default.clone()))])
        }
        SettingDefinition::EditableList { key, default, .. } => {
            HashMap::from([(key.clone(), SourceSettingValue::Vec(default.clone()))])
        }
        SettingDefinition::Switch { key, default, .. } => {
            HashMap::from([(key.clone(), SourceSettingValue::Bool(*default))])
        }
        // FIXME use `if let` guard when they become stable
        SettingDefinition::Text { key, default, .. } if default.is_some() => HashMap::from([(
            key.clone(),
            SourceSettingValue::String(default.clone().unwrap()),
        )]),
        _ => HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use crate::{settings::SourceSettingValue, source::model::SettingDefinition};
    use std::collections::HashMap;

    use super::SourceSettings;

    #[test]
    fn it_defaults_to_definition_value_if_no_stored_setting_is_present() {
        let stored_settings = HashMap::new();
        let definition = SettingDefinition::Switch {
            title: "Ok?".into(),
            key: "ok".into(),
            default: true,
        };

        let source_settings =
            SourceSettings::new("".to_owned(), &[definition], &stored_settings, None).unwrap();

        assert_eq!(
            Some(SourceSettingValue::Bool(true)),
            source_settings.get(&"ok".into())
        );
    }

    #[test]
    fn it_retrieves_stored_setting_value_if_present() {
        let mut stored_settings = HashMap::new();
        stored_settings.insert("ok".into(), SourceSettingValue::Bool(false));

        let definition = SettingDefinition::Switch {
            title: "Ok?".into(),
            key: "ok".into(),
            default: true,
        };

        let source_settings =
            SourceSettings::new("".to_owned(), &[definition], &stored_settings, None).unwrap();

        assert_eq!(
            Some(SourceSettingValue::Bool(false)),
            source_settings.get(&"ok".into())
        );
    }
}
