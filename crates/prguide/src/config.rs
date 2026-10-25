//! The reviewer's config file, `$PRGUIDE_HOME/config.toml`, and the open-command override.

use std::{
    ffi::{OsStr, OsString},
    fmt, fs, io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

pub const CONFIG_FILE: &str = "config.toml";
pub const OPEN_CMD_ENV: &str = "PRGUIDE_OPEN_CMD";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub open_command: Option<String>,
}

/// One unknown top-level key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    pub path: PathBuf,
    pub key: String,
}

impl fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ignoring unknown key `{}` in {}",
            self.key,
            self.path.display()
        )
    }
}

impl Config {
    /// Reads `<home>/config.toml`.
    pub fn load(home: &Path) -> Result<(Self, Vec<ConfigWarning>), ConfigError> {
        let path = home.join(CONFIG_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok((Self::default(), Vec::new()));
            }
            Err(source) => return Err(ConfigError::Unreadable { path, source }),
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| ConfigError::Invalid {
            path: path.clone(),
            reason: "not UTF-8".to_owned(),
        })?;
        let mut table =
            toml::from_str::<toml::Table>(text).map_err(|error| ConfigError::Invalid {
                path: path.clone(),
                reason: error.to_string().trim_end().to_owned(),
            })?;
        let open_command = match table.remove("open_command") {
            None => None,
            Some(toml::Value::String(value)) => Some(value),
            Some(_) => {
                return Err(ConfigError::Invalid {
                    path,
                    reason: "open_command must be a string".to_owned(),
                });
            }
        };
        let warnings = table
            .into_iter()
            .map(|(key, _)| ConfigWarning {
                path: path.clone(),
                key,
            })
            .collect();
        Ok((Self { open_command }, warnings))
    }
}

/// The open command to run, if any.
pub fn open_command(
    home: &Path,
    env_value: Option<&OsStr>,
) -> Result<(Option<OsString>, Vec<ConfigWarning>), ConfigError> {
    if let Some(value) = env_value.filter(|value| {
        value
            .as_bytes()
            .iter()
            .any(|byte| !byte.is_ascii_whitespace())
    }) {
        return Ok((Some(value.to_os_string()), Vec::new()));
    }
    let (config, warnings) = Config::load(home)?;
    let command = config
        .open_command
        .filter(|value| !value.trim().is_empty())
        .map(OsString::from);
    Ok((command, warnings))
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read the prguide config {}: {source}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid prguide config {}: {reason}", path.display())]
    Invalid { path: PathBuf, reason: String },
}

impl ConfigError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unreadable { .. } => "config_unreadable",
            Self::Invalid { .. } => "config_invalid",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io, path::PathBuf};

    use super::ConfigError;

    #[test]
    fn config_error_codes_are_stable() {
        assert_eq!(
            ConfigError::Unreadable {
                path: PathBuf::from("config.toml"),
                source: io::Error::other("nope"),
            }
            .code(),
            "config_unreadable"
        );
        assert_eq!(
            ConfigError::Invalid {
                path: PathBuf::from("config.toml"),
                reason: String::new(),
            }
            .code(),
            "config_invalid"
        );
    }
}
