//! Unified error type. Every IPC handler returns [`AppResult`]; the error is
//! serialized as `{ code, message }` and rethrown by the bridge as a JS `Error`
//! with Electron's message format, so renderer error handling is unchanged.

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("No handler registered for '{0}'")]
    NotImplemented(String),

    #[error("Invalid argument #{index} for '{channel}': {message}")]
    InvalidArgument {
        channel: String,
        index: usize,
        message: String,
    },

    #[error("Not authenticated")]
    Unauthenticated,

    #[error("Roblox API error {status} at {url}: {message}")]
    Api {
        status: u16,
        url: String,
        message: String,
    },

    #[error("{0}")]
    Message(String),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Tauri(#[from] tauri::Error),
}

impl AppError {
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    /// Stable machine-readable code exposed to the renderer.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotImplemented(_) => "NOT_IMPLEMENTED",
            Self::InvalidArgument { .. } => "INVALID_ARGUMENT",
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::Api { status: 429, .. } => "RATE_LIMITED",
            Self::Api { .. } => "API_ERROR",
            Self::Message(_) => "ERROR",
            Self::Http(_) => "NETWORK_ERROR",
            Self::Io(_) => "IO_ERROR",
            Self::Json(_) => "JSON_ERROR",
            Self::Tauri(_) => "RUNTIME_ERROR",
        }
    }
}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("AppError", 2)?;
        state.serialize_field("code", self.code())?;
        state.serialize_field("message", &self.to_string())?;
        state.end()
    }
}
