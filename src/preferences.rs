use std::path::Path;

use serde::{Deserialize, Serialize};

/// Shared default audio device selections stored by Maolan applications.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioDevicePreferences {
    default_output_device_id: Option<String>,
    default_input_device_id: Option<String>,
}

impl AudioDevicePreferences {
    pub fn from_toml_str(contents: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(contents)
    }

    pub fn default_output_device_id(&self) -> Option<&str> {
        self.default_output_device_id.as_deref()
    }

    pub fn default_input_device_id(&self) -> Option<&str> {
        self.default_input_device_id.as_deref()
    }

    #[cfg(target_os = "freebsd")]
    pub fn set_default_output_device_id(&mut self, id: Option<String>) {
        self.default_output_device_id = id;
    }

    #[cfg(target_os = "freebsd")]
    pub fn set_default_input_device_id(&mut self, id: Option<String>) {
        self.default_input_device_id = id;
    }

    /// Replaces these device settings in a TOML document while preserving its other values.
    #[cfg(target_os = "freebsd")]
    pub fn update_toml_str(&self, contents: &str) -> Result<String, String> {
        let mut value = if contents.trim().is_empty() {
            toml::Value::Table(toml::map::Map::new())
        } else {
            toml::from_str::<toml::Value>(contents).map_err(|error| error.to_string())?
        };
        let table = value
            .as_table_mut()
            .ok_or_else(|| String::from("Audio device settings require a TOML table."))?;
        update_id(
            table,
            "default_output_device_id",
            &self.default_output_device_id,
        );
        update_id(
            table,
            "default_input_device_id",
            &self.default_input_device_id,
        );
        toml::to_string_pretty(&value).map_err(|error| error.to_string())
    }

    pub fn load_from_path(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|contents| Self::from_toml_str(&contents).ok())
            .unwrap_or_default()
    }

    #[cfg(target_os = "freebsd")]
    pub fn save_to_path(&self, path: &Path) -> Result<(), String> {
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.to_string()),
        };
        let updated = self.update_toml_str(&contents)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(path, updated).map_err(|error| error.to_string())
    }
}

#[cfg(target_os = "freebsd")]
fn update_id(table: &mut toml::map::Map<String, toml::Value>, key: &str, id: &Option<String>) {
    if let Some(id) = id
        .as_deref()
        .filter(|id| !id.is_empty() && *id != "__auto__")
    {
        table.insert(key.to_string(), toml::Value::String(id.to_string()));
    } else {
        table.remove(key);
    }
}

#[cfg(all(test, target_os = "freebsd"))]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "freebsd")]
    fn update_toml_preserves_other_preferences() {
        let mut preferences = AudioDevicePreferences::default();
        preferences.set_default_output_device_id(Some(String::from("/dev/dsp5")));
        preferences.set_default_input_device_id(Some(String::from("/dev/dsp5")));
        let updated = preferences
            .update_toml_str("theme = \"dark\"\ndefault_output_device_id = \"out2\"\n")
            .unwrap();
        let parsed = AudioDevicePreferences::from_toml_str(&updated).unwrap();
        assert_eq!(parsed, preferences);
        assert!(updated.contains("theme = \"dark\""));
    }

    #[test]
    #[cfg(target_os = "freebsd")]
    fn update_toml_removes_unset_device_ids() {
        let updated = AudioDevicePreferences::default()
            .update_toml_str(
                "default_output_device_id = \"old\"\ndefault_input_device_id = \"old\"\n",
            )
            .unwrap();
        assert!(!updated.contains("default_output_device_id"));
        assert!(!updated.contains("default_input_device_id"));
    }
}
