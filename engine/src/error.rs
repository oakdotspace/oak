use serde_json::{json, Map, Value};

/// A typed failure: the `error` object of an `ok: false` response. Fields past
/// `code`, `message` and `retryable` are additive detail, such as the
/// `conflict_paths` of a conflict.
#[derive(Debug)]
pub struct EngineError {
    code: &'static str,
    message: String,
    retryable: bool,
    detail: Map<String, Value>,
}

impl EngineError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        EngineError {
            code,
            message: message.into(),
            retryable: false,
            detail: Map::new(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn detail(mut self, key: &str, value: Value) -> Self {
        self.detail.insert(key.to_string(), value);
        self
    }

    pub fn to_json(&self) -> Value {
        let mut error = json!({
            "code": self.code,
            "message": self.message,
            "retryable": self.retryable,
        });
        if let Some(object) = error.as_object_mut() {
            object.extend(self.detail.clone());
        }
        error
    }
}
