//! Persisted theme preferences for the Dark Factory window.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
};

use jg_ui::{ThemePref, ThemeStyle};
use serde::Deserialize;

const FILE_NAME: &str = "appearance.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appearance {
    pub theme: ThemePref,
    pub style: ThemeStyle,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemePref::System,
            style: ThemeStyle::Default,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StoredAppearance {
    theme: String,
    style: String,
}

impl Default for StoredAppearance {
    fn default() -> Self {
        Self {
            theme: ThemePref::System.as_str().to_string(),
            style: ThemeStyle::Default.as_str().to_string(),
        }
    }
}

/// Loads appearance preferences from `data_dir`; absent or invalid preferences use defaults.
pub fn load(data_dir: &Path) -> Appearance {
    let contents = match fs::read_to_string(data_dir.join(FILE_NAME)) {
        Ok(contents) => contents,
        Err(_) => return Appearance::default(),
    };
    let stored: StoredAppearance = match toml::from_str(&contents) {
        Ok(stored) => stored,
        Err(_) => return Appearance::default(),
    };

    let Some(theme) = parse_theme(&stored.theme) else {
        return Appearance::default();
    };
    let Some(style) = parse_style(&stored.style) else {
        return Appearance::default();
    };

    Appearance { theme, style }
}

/// Saves preferences with a same-directory temporary file and rename.
pub fn save(data_dir: &Path, appearance: Appearance) -> io::Result<()> {
    fs::create_dir_all(data_dir)?;

    let destination = data_dir.join(FILE_NAME);
    let temporary = data_dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
    let contents = format!(
        "theme = \"{}\"\nstyle = \"{}\"\n",
        appearance.theme.as_str(),
        appearance.style.as_str()
    );

    let result = write_and_replace(&temporary, &destination, contents.as_bytes());
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn parse_theme(value: &str) -> Option<ThemePref> {
    match value {
        "system" | "dark" | "light" => Some(ThemePref::parse(value)),
        _ => None,
    }
}

fn parse_style(value: &str) -> Option<ThemeStyle> {
    match value {
        "default" | "jg-violet" | "violet" | "tokyo-night" | "dracula" => {
            Some(ThemeStyle::parse(value))
        }
        _ => None,
    }
}

fn write_and_replace(temporary: &Path, destination: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_uses_system_and_default_style() {
        let directory = tempfile::tempdir().expect("temporary data directory");

        assert_eq!(load(directory.path()), Appearance::default());
    }

    #[test]
    fn valid_file_loads_supported_preferences() {
        let directory = tempfile::tempdir().expect("temporary data directory");
        fs::write(
            directory.path().join(FILE_NAME),
            "theme = \"light\"\nstyle = \"tokyo-night\"\n",
        )
        .expect("write appearance file");

        assert_eq!(
            load(directory.path()),
            Appearance {
                theme: ThemePref::Light,
                style: ThemeStyle::TokyoNight,
            }
        );
    }

    #[test]
    fn legacy_violet_style_name_is_accepted() {
        let directory = tempfile::tempdir().expect("temporary data directory");
        fs::write(
            directory.path().join(FILE_NAME),
            "theme = \"dark\"\nstyle = \"violet\"\n",
        )
        .expect("write appearance file");

        assert_eq!(
            load(directory.path()),
            Appearance {
                theme: ThemePref::Dark,
                style: ThemeStyle::Violet,
            }
        );
    }

    #[test]
    fn invalid_file_uses_defaults() {
        let directory = tempfile::tempdir().expect("temporary data directory");
        fs::write(directory.path().join(FILE_NAME), "theme = [\n").expect("write malformed file");
        assert_eq!(load(directory.path()), Appearance::default());

        fs::write(
            directory.path().join(FILE_NAME),
            "theme = \"dark\"\nstyle = \"not-a-style\"\n",
        )
        .expect("write appearance file");

        assert_eq!(load(directory.path()), Appearance::default());
    }

    #[test]
    fn save_writes_parseable_values_and_loads_them_back() {
        let directory = tempfile::tempdir().expect("temporary data directory");
        let appearance = Appearance {
            theme: ThemePref::Dark,
            style: ThemeStyle::Violet,
        };

        save(directory.path(), appearance).expect("save appearance");

        assert_eq!(load(directory.path()), appearance);
        let contents =
            fs::read_to_string(directory.path().join(FILE_NAME)).expect("read appearance file");
        assert!(contents.contains(&format!("style = \"{}\"", ThemeStyle::Violet.as_str())));
        assert!(
            !directory
                .path()
                .join(format!(".{FILE_NAME}.{}.tmp", std::process::id()))
                .exists()
        );
    }
}
