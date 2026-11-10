//! Positional IPC arguments (`ipcRenderer.invoke(channel, ...args)`).
//! Values are deserialized by reference straight from the JSON tree, so no
//! intermediate clone of the payload is made.

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::error::{AppError, AppResult};

pub struct Args {
    channel: String,
    values: Vec<Value>,
}

impl Args {
    pub fn new(channel: String, values: Vec<Value>) -> Self {
        Self { channel, values }
    }

    /// Required argument at `index`.
    pub fn get<T: DeserializeOwned>(&self, index: usize) -> AppResult<T> {
        let value = self.values.get(index).unwrap_or(&Value::Null);
        T::deserialize(value).map_err(|error| self.invalid(index, error))
    }

    /// Optional argument: missing or `null`/`undefined` yields `None`.
    pub fn opt<T: DeserializeOwned>(&self, index: usize) -> AppResult<Option<T>> {
        match self.values.get(index) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => Option::<T>::deserialize(value).map_err(|error| self.invalid(index, error)),
        }
    }

    /// Moves an argument out (for pass-through payloads such as patches).
    pub fn take(&mut self, index: usize) -> Value {
        self.values.get_mut(index).map(std::mem::take).unwrap_or(Value::Null)
    }

    fn invalid(&self, index: usize, error: serde_json::Error) -> AppError {
        AppError::InvalidArgument {
            channel: self.channel.clone(),
            index,
            message: error.to_string(),
        }
    }
}
