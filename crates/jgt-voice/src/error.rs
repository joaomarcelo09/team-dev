use std::path::PathBuf;

#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum VoiceError {
    #[error("missing model {what} at {}: {hint}", path.display())]
    MissingModel {
        what: String,
        path: PathBuf,
        hint: String,
    },
    #[error("command not found '{command}': {hint}")]
    CommandNotFound {
        command: String,
        hint: String,
    },
    #[error("audio device matching '{name}' not found; available: [{}]", available.join(", "))]
    DeviceNotFound {
        name: String,
        available: Vec<String>,
    },
    #[error("{0}")]
    Other(String),
}
