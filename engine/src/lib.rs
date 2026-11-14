//! `oak_engine`: the logic the Godot, Unity and Unreal extensions share.
//!
//! Every operation is one JSON request and one JSON response through
//! [`oak_engine_call`]. The library runs no threads or processes and does no
//! network I/O: `cli.plan` says what to run, the extension runs it with its
//! own async primitive, and `cli.parse` says what the result means.
//! `docs/engine-contract.md` is the contract.

mod args;
mod assets;
mod binary;
mod cli_parse;
mod cli_plan;
mod error;
mod text;
mod version;

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

use serde_json::{json, Value};

use args::Args;
use error::EngineError;

/// The ABI version, also the request and response envelope's `v`.
const ABI_VERSION: u64 = 1;
const ABI_VERSION_CSTR: &CStr = c"1";

/// Handle one request and return the response as JSON text. The safe core of
/// [`oak_engine_call`], and what the tests drive.
pub fn call(request_json: &str) -> String {
    let outcome =
        catch_unwind(AssertUnwindSafe(|| dispatch_request(request_json))).unwrap_or_else(|_| {
            Err(EngineError::new(
                "internal",
                "oak_engine panicked while handling this request",
            ))
        });
    envelope(outcome).to_string()
}

fn envelope(outcome: Result<Value, EngineError>) -> Value {
    match outcome {
        Ok(result) => json!({"v": ABI_VERSION, "ok": true, "result": result}),
        Err(error) => json!({"v": ABI_VERSION, "ok": false, "error": error.to_json()}),
    }
}

fn dispatch_request(request_json: &str) -> Result<Value, EngineError> {
    let request: Value = serde_json::from_str(request_json)
        .map_err(|e| EngineError::bad_request(format!("request is not JSON: {e}")))?;
    if !request.is_object() {
        return Err(EngineError::bad_request("request must be a JSON object"));
    }
    let version = request.get("v").and_then(Value::as_u64);
    if version != Some(ABI_VERSION) {
        return Err(EngineError::new(
            "unsupported_version",
            format!("this library speaks v{ABI_VERSION}; the request has {version:?}"),
        ));
    }
    let op = request
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| EngineError::bad_request("`op` must be a string"))?;
    let empty = json!({});
    let args = Args::new(request.get("args").unwrap_or(&empty))?;
    dispatch(op, &args)
}

fn dispatch(op: &str, args: &Args) -> Result<Value, EngineError> {
    match op {
        "engine.detect" => assets::detect(args),
        "ignore.template" => assets::ignore_template(args),
        "assets.classify" => assets::classify(args),
        "assets.companions" => assets::companions(args),
        "status.rollup" => text::status_rollup(args),
        "diff.text" => text::diff_text(args),
        "merge.text3" => text::merge_text3(args),
        "narrative.append" => text::narrative_append(args),
        "binary.candidates" => binary::candidates(args),
        "version.check" => version::check(args),
        "cli.plan" => cli_plan::plan(args),
        "cli.parse" => cli_parse::parse(args),
        // Only reachable from tests: proves a panic stays inside the ABI.
        #[cfg(test)]
        "test.panic" => panic!("requested panic"),
        _ => Err(EngineError::new("unknown_op", format!("unknown op `{op}`"))),
    }
}

/// The ABI version as a static NUL-terminated string. Never freed.
#[no_mangle]
pub extern "C" fn oak_engine_abi_version() -> *const c_char {
    ABI_VERSION_CSTR.as_ptr()
}

/// Handle one UTF-8 JSON request. Thread-safe, and never returns NULL: every
/// failure, including a panic, is an `ok: false` response. Free the result
/// with [`oak_engine_free`].
///
/// # Safety
///
/// `request_json` is NULL or points to a NUL-terminated string that stays
/// valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn oak_engine_call(request_json: *const c_char) -> *mut c_char {
    let response = catch_unwind(AssertUnwindSafe(|| {
        if request_json.is_null() {
            return envelope(Err(EngineError::bad_request("request is NULL"))).to_string();
        }
        // SAFETY: non-NULL and NUL-terminated, per this function's contract.
        match unsafe { CStr::from_ptr(request_json) }.to_str() {
            Ok(request) => call(request),
            Err(_) => envelope(Err(EngineError::bad_request("request is not UTF-8"))).to_string(),
        }
    }))
    .unwrap_or_else(|_| {
        envelope(Err(EngineError::new("internal", "oak_engine panicked"))).to_string()
    });
    // serde_json escapes NUL as \u0000, so a response never holds a raw one.
    CString::new(response).unwrap_or_default().into_raw()
}

/// Free a response from [`oak_engine_call`]. NULL is a no-op.
///
/// # Safety
///
/// `response_json` is NULL or a pointer [`oak_engine_call`] returned, and is
/// freed at most once.
#[no_mangle]
pub unsafe extern "C" fn oak_engine_free(response_json: *mut c_char) {
    if !response_json.is_null() {
        // SAFETY: it came from CString::into_raw in oak_engine_call.
        drop(unsafe { CString::from_raw(response_json) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(request: &str) -> Value {
        serde_json::from_str(&call(request)).unwrap()
    }

    fn error_code(request: &str) -> String {
        let response = response(request);
        assert_eq!(response["ok"], false, "{response}");
        assert_eq!(response["v"], 1);
        response["error"]["code"].as_str().unwrap().to_string()
    }

    #[test]
    fn envelope_errors() {
        assert_eq!(error_code("not json"), "bad_request");
        assert_eq!(error_code("[]"), "bad_request");
        assert_eq!(error_code(r#"{"v": 1}"#), "bad_request");
        assert_eq!(error_code(r#"{"op": "x"}"#), "unsupported_version");
        assert_eq!(error_code(r#"{"v": 2, "op": "x"}"#), "unsupported_version");
        assert_eq!(error_code(r#"{"v": 1, "op": "nope"}"#), "unknown_op");
        assert_eq!(
            error_code(r#"{"v": 1, "op": "diff.text", "args": []}"#),
            "bad_request"
        );
    }

    #[test]
    fn a_panic_does_not_cross_the_abi() {
        std::panic::set_hook(Box::new(|_| {}));
        let response = response(r#"{"v": 1, "op": "test.panic"}"#);
        let _ = std::panic::take_hook();
        assert_eq!(response["error"]["code"], "internal");
        assert_eq!(response["error"]["retryable"], false);
    }

    #[test]
    fn c_abi_round_trip() {
        let version = unsafe { CStr::from_ptr(oak_engine_abi_version()) };
        assert_eq!(version.to_str().unwrap(), "1");

        let request =
            CString::new(r#"{"v":1,"op":"version.check","args":{"cli_version":"oak 9.9.9"}}"#);
        let raw = unsafe { oak_engine_call(request.unwrap().as_ptr()) };
        assert!(!raw.is_null());
        let text = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
        unsafe { oak_engine_free(raw) };
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["result"]["ok"], true);
    }

    #[test]
    fn c_abi_rejects_null_and_bad_utf8() {
        let raw = unsafe { oak_engine_call(std::ptr::null()) };
        let text = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
        unsafe { oak_engine_free(raw) };
        assert!(text.contains("bad_request"));

        let bad = [0xff_u8, 0xfe, 0];
        let raw = unsafe { oak_engine_call(bad.as_ptr().cast()) };
        let text = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
        unsafe { oak_engine_free(raw) };
        assert!(text.contains("not UTF-8"));

        unsafe { oak_engine_free(std::ptr::null_mut()) };
    }

    #[test]
    fn calls_are_thread_safe() {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let request = format!(
                            r#"{{"v":1,"op":"narrative.append","args":{{"existing":"{i}","message":"x"}}}}"#
                        );
                        let response: Value = serde_json::from_str(&call(&request)).unwrap();
                        assert_eq!(response["result"]["text"], format!("{i}\n\nx"));
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    }
}
