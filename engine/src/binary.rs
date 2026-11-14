use serde_json::{json, Value};

use crate::args::Args;
use crate::error::EngineError;

/// Where to look for `oak`, in order. Nothing here touches the filesystem: the
/// extension takes the first candidate that exists. GUI apps on macOS often
/// run with a stripped PATH, hence the fixed locations after it.
pub fn candidates(args: &Args) -> Result<Value, EngineError> {
    let platform = args.str("platform")?;
    let windows = match platform {
        "windows" => true,
        "darwin" | "linux" => false,
        other => {
            return Err(EngineError::bad_request(format!(
                "unknown platform `{other}`: expected windows, darwin or linux"
            )))
        }
    };
    let home = args.opt_str("home")?.unwrap_or("");
    let override_path = args.opt_str("override")?.unwrap_or("").trim();
    let path_env = args.opt_str("path_env")?.unwrap_or("");

    let (separator, list_separator, exe) = if windows {
        ('\\', ';', "oak.exe")
    } else {
        ('/', ':', "oak")
    };
    let join = |dir: &str, leaf: &str| {
        format!(
            "{}{separator}{leaf}",
            dir.trim_end_matches(['/', '\\'].as_slice())
        )
    };

    let mut found: Vec<String> = Vec::new();
    if !override_path.is_empty() {
        found.push(override_path.to_string());
    }
    for dir in path_env.split(list_separator) {
        // An empty PATH entry means the cwd, which is never a safe place to
        // find a binary from inside an editor.
        let dir = dir.trim().trim_matches('"');
        if !dir.is_empty() {
            found.push(join(dir, exe));
        }
    }
    if !home.is_empty() {
        let bin = |sub: &str| join(&join(&join(home, sub), "bin"), exe);
        found.push(bin(".local"));
        found.push(bin(".cargo"));
    }
    if !windows {
        found.push("/usr/local/bin/oak".to_string());
        found.push("/opt/homebrew/bin/oak".to_string());
    }

    let mut seen = std::collections::HashSet::new();
    found.retain(|path| seen.insert(path.clone()));
    Ok(json!({ "candidates": found }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: Value) -> Result<Vec<String>, EngineError> {
        let result = candidates(&Args::new(&args).unwrap())?;
        Ok(result["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect())
    }

    #[test]
    fn unix_order_and_dedupe() {
        let found = run(json!({
            "platform": "darwin",
            "home": "/Users/ada/",
            "override": " /opt/oak ",
            "path_env": "/usr/bin::/Users/ada/.local/bin/",
        }))
        .unwrap();
        assert_eq!(
            found,
            [
                "/opt/oak",
                "/usr/bin/oak",
                "/Users/ada/.local/bin/oak",
                "/Users/ada/.cargo/bin/oak",
                "/usr/local/bin/oak",
                "/opt/homebrew/bin/oak",
            ]
        );
    }

    #[test]
    fn windows_paths() {
        let found = run(json!({
            "platform": "windows",
            "home": "C:\\Users\\ada",
            "path_env": "C:\\tools;\"C:\\Program Files\\oak\"",
        }))
        .unwrap();
        assert_eq!(
            found,
            [
                "C:\\tools\\oak.exe",
                "C:\\Program Files\\oak\\oak.exe",
                "C:\\Users\\ada\\.local\\bin\\oak.exe",
                "C:\\Users\\ada\\.cargo\\bin\\oak.exe",
            ]
        );
    }

    #[test]
    fn no_home_and_bad_platform() {
        assert_eq!(run(json!({"platform": "linux"})).unwrap().len(), 2);
        assert!(run(json!({"platform": "beos"})).is_err());
    }
}
