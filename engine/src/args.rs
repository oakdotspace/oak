use oak_core::GameEngine;
use serde_json::Value;

use crate::error::EngineError;

/// Typed access to a request's `args`. An absent field and an explicit `null`
/// mean the same thing, matching Oak's JSON rule.
pub struct Args<'a>(&'a Value);

impl<'a> Args<'a> {
    pub fn new(value: &'a Value) -> Result<Self, EngineError> {
        if value.is_object() {
            Ok(Args(value))
        } else {
            Err(EngineError::bad_request("`args` must be a JSON object"))
        }
    }

    fn value(&self, key: &str) -> Option<&'a Value> {
        self.0.get(key).filter(|value| !value.is_null())
    }

    /// A nested object, such as `cli.plan`'s `params`. Absent means empty.
    pub fn object(&self, key: &str) -> Result<Args<'a>, EngineError> {
        // `get` on Null finds nothing, which is what an empty object needs.
        static EMPTY: Value = Value::Null;
        match self.value(key) {
            Some(value) if value.is_object() => Ok(Args(value)),
            Some(_) => Err(EngineError::bad_request(format!(
                "`{key}` must be an object"
            ))),
            None => Ok(Args(&EMPTY)),
        }
    }

    pub fn opt_str(&self, key: &str) -> Result<Option<&'a str>, EngineError> {
        match self.value(key) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .map(Some)
                .ok_or_else(|| EngineError::bad_request(format!("`{key}` must be a string"))),
        }
    }

    pub fn str(&self, key: &str) -> Result<&'a str, EngineError> {
        self.opt_str(key)?
            .ok_or_else(|| EngineError::bad_request(format!("`{key}` is required")))
    }

    pub fn opt_u64(&self, key: &str) -> Result<Option<u64>, EngineError> {
        match self.value(key) {
            None => Ok(None),
            Some(value) => value.as_u64().map(Some).ok_or_else(|| {
                EngineError::bad_request(format!("`{key}` must be a non-negative integer"))
            }),
        }
    }

    pub fn u64(&self, key: &str) -> Result<u64, EngineError> {
        self.opt_u64(key)?
            .ok_or_else(|| EngineError::bad_request(format!("`{key}` is required")))
    }

    pub fn i64(&self, key: &str) -> Result<i64, EngineError> {
        self.value(key)
            .and_then(Value::as_i64)
            .ok_or_else(|| EngineError::bad_request(format!("`{key}` must be an integer")))
    }

    pub fn bool(&self, key: &str) -> Result<bool, EngineError> {
        match self.value(key) {
            None => Ok(false),
            Some(value) => value
                .as_bool()
                .ok_or_else(|| EngineError::bad_request(format!("`{key}` must be a boolean"))),
        }
    }

    pub fn array(&self, key: &str) -> Result<&'a Vec<Value>, EngineError> {
        self.value(key)
            .and_then(Value::as_array)
            .ok_or_else(|| EngineError::bad_request(format!("`{key}` must be an array")))
    }

    /// An array of strings. Absent means empty.
    pub fn str_list(&self, key: &str) -> Result<Vec<&'a str>, EngineError> {
        if self.value(key).is_none() {
            return Ok(Vec::new());
        }
        self.array(key)?
            .iter()
            .map(|item| {
                item.as_str().ok_or_else(|| {
                    EngineError::bad_request(format!("`{key}` must hold only strings"))
                })
            })
            .collect()
    }

    pub fn engine(&self) -> Result<GameEngine, EngineError> {
        match self.str("engine")? {
            "godot" => Ok(GameEngine::Godot),
            "unity" => Ok(GameEngine::Unity),
            "unreal" => Ok(GameEngine::Unreal),
            other => Err(EngineError::bad_request(format!(
                "unknown engine `{other}`: expected godot, unity or unreal"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn null_is_absent_and_types_are_checked() {
        let value = json!({"s": null, "n": "x", "b": 1, "l": [1], "o": 1, "i": "x"});
        let args = Args::new(&value).unwrap();
        assert_eq!(args.opt_str("s").unwrap(), None);
        assert!(args.str("s").is_err());
        assert!(args.opt_str("n").is_ok());
        assert!(args.opt_u64("n").is_err());
        assert!(args.u64("missing").is_err());
        assert!(args.i64("i").is_err());
        assert!(args.bool("b").is_err());
        assert!(!args.bool("missing").unwrap());
        assert!(args.str_list("l").is_err());
        assert!(args.str_list("missing").unwrap().is_empty());
        assert!(args.array("missing").is_err());
        assert!(args.object("o").is_err());
        assert!(args.object("missing").is_ok());
        assert!(args.opt_str("b").is_err());
        assert!(args.engine().is_err());
    }
}
