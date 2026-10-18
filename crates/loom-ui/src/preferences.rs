//! Client-local presentation preferences.
//!
//! These settings describe how this client renders agent output. They are not
//! workspace state: the backend never sees them and they do not travel to other
//! clients or workers. What the backend does see is the activity projection a
//! level implies for one read (`activity_detail` on the requests that carry
//! transcript content), never the setting itself. Native builds keep them in
//! `config_dir()`; the browser build keeps them in `localStorage`.

use loom_core::{ErrorCode, LoomError};
use serde::{Deserialize, Serialize};

use crate::{
    state::{OutputDetail, ThemeChoice},
    view::{DEFAULT_FONT_SCALE_PERCENT, MAX_FONT_SCALE_PERCENT, MIN_FONT_SCALE_PERCENT},
};

/// The presentation settings that survive a client restart.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct UiPreferences {
    pub(crate) show_reasoning: bool,
    pub(crate) output_detail: OutputDetail,
    pub(crate) theme_choice: ThemeChoice,
    pub(crate) font_scale_percent: u16,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            show_reasoning: false,
            output_detail: OutputDetail::default(),
            theme_choice: ThemeChoice::default(),
            font_scale_percent: DEFAULT_FONT_SCALE_PERCENT,
        }
    }
}

impl UiPreferences {
    /// Keeps a hand-edited or older file inside the range the Appearance
    /// controls can represent.
    fn normalized(mut self) -> Self {
        self.font_scale_percent = self
            .font_scale_percent
            .clamp(MIN_FONT_SCALE_PERCENT, MAX_FONT_SCALE_PERCENT);
        self
    }

    #[cfg(all(not(target_family = "wasm"), not(test)))]
    pub(crate) fn load() -> Self {
        let path = native_path();
        match Self::load_from(&path) {
            Ok(preferences) => preferences,
            Err(error) => {
                log::warn!(
                    "could not load UI preferences from '{}': {error}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    #[cfg(all(not(target_family = "wasm"), not(test)))]
    pub(crate) fn save(self) {
        let path = native_path();
        if let Err(error) = Self::save_to(&path, self) {
            log::warn!(
                "could not save UI preferences to '{}': {error}",
                path.display()
            );
        }
    }

    #[cfg(all(target_family = "wasm", not(test)))]
    pub(crate) fn load() -> Self {
        match wasm::load() {
            Ok(preferences) => preferences,
            Err(error) => {
                log::warn!("could not load UI preferences from browser storage: {error}");
                Self::default()
            }
        }
    }

    #[cfg(all(target_family = "wasm", not(test)))]
    pub(crate) fn save(self) {
        if let Err(error) = wasm::save(self) {
            log::warn!("could not save UI preferences to browser storage: {error}");
        }
    }

    /// Tests never read or write the developer's real preferences; the file
    /// helpers below are exercised directly with temporary paths instead.
    #[cfg(test)]
    pub(crate) fn load() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn save(self) {
        let _ = self;
    }
}

#[cfg(not(target_family = "wasm"))]
const PREFERENCES_FILE_NAME: &str = "preferences.json";

#[cfg(all(not(target_family = "wasm"), not(test)))]
fn native_path() -> std::path::PathBuf {
    loom_core::config_dir().join(PREFERENCES_FILE_NAME)
}

#[cfg(not(target_family = "wasm"))]
impl UiPreferences {
    fn load_from(path: &Path) -> loom_core::Result<Self> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let bytes = fs::read(path).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!(
                    "could not read UI preferences '{}': {error}",
                    path.display()
                ),
                false,
            )
        })?;
        let preferences: Self = serde_json::from_slice(&bytes).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "UI preferences '{}' contain invalid JSON: {error}",
                    path.display()
                ),
                false,
            )
        })?;
        Ok(preferences.normalized())
    }

    fn save_to(path: &Path, preferences: Self) -> loom_core::Result<()> {
        let preferences = preferences.normalized();
        let parent = path.parent().ok_or_else(|| {
            LoomError::invalid_request("UI preferences path must have a parent directory")
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!(
                    "could not create the UI preferences directory '{}': {error}",
                    parent.display()
                ),
                false,
            )
        })?;
        let bytes = serde_json::to_vec_pretty(&preferences).map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not encode UI preferences: {error}"),
                false,
            )
        })?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, bytes).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!(
                    "could not write UI preferences '{}': {error}",
                    temporary.display()
                ),
                false,
            )
        })?;
        fs::rename(&temporary, path).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!(
                    "could not replace UI preferences '{}': {error}",
                    path.display()
                ),
                false,
            )
        })
    }
}

#[cfg(all(target_family = "wasm", not(test)))]
mod wasm {
    use super::*;
    use crate::browser::{browser_storage, describe_js};

    const STORAGE_KEY: &str = "loom.ui_preferences";

    pub(super) fn load() -> loom_core::Result<UiPreferences> {
        let storage = browser_storage()?;
        let Some(serialized) = storage.get_item(STORAGE_KEY).map_err(|error| {
            LoomError::new(
                ErrorCode::Persistence,
                format!(
                    "could not read UI preferences from browser storage: {}",
                    describe_js(&error)
                ),
                false,
            )
        })?
        else {
            return Ok(UiPreferences::default());
        };
        let preferences: UiPreferences = serde_json::from_str(&serialized).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("saved UI preferences are malformed: {error}"),
                false,
            )
        })?;
        Ok(preferences.normalized())
    }

    pub(super) fn save(preferences: UiPreferences) -> loom_core::Result<()> {
        let serialized = serde_json::to_string(&preferences.normalized()).map_err(|error| {
            LoomError::new(
                ErrorCode::Internal,
                format!("could not encode UI preferences: {error}"),
                false,
            )
        })?;
        browser_storage()?
            .set_item(STORAGE_KEY, &serialized)
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Persistence,
                    format!(
                        "could not save UI preferences in browser storage: {}",
                        describe_js(&error)
                    ),
                    false,
                )
            })
    }
}

#[cfg(not(target_family = "wasm"))]
use std::{fs, path::Path};

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    fn temporary_path() -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("loom-ui-preferences-{}", uuid::Uuid::new_v4()))
            .join(PREFERENCES_FILE_NAME)
    }

    fn remove_parent(path: &Path) {
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn missing_file_loads_defaults() {
        let path = temporary_path();
        assert_eq!(
            UiPreferences::load_from(&path).unwrap(),
            UiPreferences::default()
        );
    }

    #[test]
    fn preferences_round_trip() {
        let path = temporary_path();
        let preferences = UiPreferences {
            show_reasoning: true,
            output_detail: OutputDetail::High,
            theme_choice: ThemeChoice::Dark,
            font_scale_percent: 120,
        };

        UiPreferences::save_to(&path, preferences).unwrap();

        let serialized = fs::read_to_string(&path).unwrap();
        assert!(serialized.contains("\"output_detail\": \"high\""));
        assert!(serialized.contains("\"theme_choice\": \"dark\""));
        assert_eq!(UiPreferences::load_from(&path).unwrap(), preferences);
        assert!(!path.with_extension("json.tmp").exists());
        remove_parent(&path);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let path = temporary_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"show_reasoning":true,"output_detail":"low","future_setting":7}"#,
        )
        .unwrap();

        assert_eq!(
            UiPreferences::load_from(&path).unwrap(),
            UiPreferences {
                show_reasoning: true,
                output_detail: OutputDetail::Low,
                ..UiPreferences::default()
            }
        );
        remove_parent(&path);
    }

    #[test]
    fn font_scale_is_clamped_to_the_appearance_range() {
        let path = temporary_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        fs::write(&path, r#"{"font_scale_percent":999}"#).unwrap();
        assert_eq!(
            UiPreferences::load_from(&path).unwrap(),
            UiPreferences {
                font_scale_percent: MAX_FONT_SCALE_PERCENT,
                ..UiPreferences::default()
            }
        );

        fs::write(&path, r#"{"font_scale_percent":1}"#).unwrap();
        assert_eq!(
            UiPreferences::load_from(&path).unwrap(),
            UiPreferences {
                font_scale_percent: MIN_FONT_SCALE_PERCENT,
                ..UiPreferences::default()
            }
        );
        remove_parent(&path);
    }

    #[test]
    fn malformed_file_is_reported() {
        let path = temporary_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();

        let error = UiPreferences::load_from(&path).unwrap_err();
        assert_eq!(error.code, ErrorCode::MalformedPayload);
        remove_parent(&path);
    }

    #[test]
    fn a_path_without_a_parent_is_rejected() {
        let error = UiPreferences::save_to(Path::new("/"), UiPreferences::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn a_blocked_directory_is_reported() {
        let root =
            std::env::temp_dir().join(format!("loom-ui-preferences-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let blocking = root.join("blocking");
        fs::write(&blocking, "not a directory").unwrap();

        let error = UiPreferences::save_to(
            &blocking.join(PREFERENCES_FILE_NAME),
            UiPreferences::default(),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Persistence);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_replace_is_reported() {
        let root =
            std::env::temp_dir().join(format!("loom-ui-preferences-{}", uuid::Uuid::new_v4()));
        let path = root.join("preferences");
        fs::create_dir_all(&path).unwrap();

        let error = UiPreferences::save_to(&path, UiPreferences::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::Persistence);
        fs::remove_file(path.with_extension("json.tmp")).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}
