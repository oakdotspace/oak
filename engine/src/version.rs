use serde_json::{json, Value};

use crate::args::Args;
use crate::error::EngineError;

/// The oldest CLI the plans work against: 0.110.0 added `oak ci publish`.
const MINIMUM: (u64, u64, u64) = (0, 110, 0);
/// Feature flags for extensions, each with the release that introduced it.
/// Everything at or above [`MINIMUM`] is listed unconditionally by comparison,
/// so this stays meaningful when the minimum drops.
const CAPABILITIES: &[(&str, (u64, u64, u64))] =
    &[("desc_append", (0, 104, 0)), ("ci_publish", (0, 110, 0))];

/// The parts of `oak --version` output: `oak 0.110.0 (nightly 20261106.274889b0)`.
pub struct CliVersion {
    pub version: String,
    pub triple: (u64, u64, u64),
    pub channel: String,
    pub build: Option<String>,
}

pub fn parse_cli_version(text: &str) -> Option<CliVersion> {
    let mut words = text.split_whitespace().peekable();
    if words.peek() == Some(&"oak") {
        words.next();
    }
    let version = words.next()?.trim_start_matches('v').to_string();
    // A prerelease suffix sorts with its release: only the triple matters.
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>());
    let triple = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    if parts.next().is_some() {
        return None;
    }
    let channel = words.next().map(|word| word.trim_start_matches('('));
    let build = words
        .next()
        .map(|word| word.trim_end_matches(')').to_string());
    Some(CliVersion {
        version,
        triple,
        channel: channel.unwrap_or("stable").to_string(),
        build,
    })
}

pub fn check(args: &Args) -> Result<Value, EngineError> {
    let raw = args.str("cli_version")?;
    let minimum = format!("{}.{}.{}", MINIMUM.0, MINIMUM.1, MINIMUM.2);
    let Some(parsed) = parse_cli_version(raw) else {
        return Ok(json!({
            "ok": false,
            "minimum": minimum,
            "capabilities": [],
            "version": null,
        }));
    };
    let capabilities: Vec<&str> = CAPABILITIES
        .iter()
        .filter(|(_, since)| parsed.triple >= *since)
        .map(|(name, _)| *name)
        .collect();
    Ok(json!({
        "ok": parsed.triple >= MINIMUM,
        "minimum": minimum,
        "capabilities": capabilities,
        "version": parsed.version,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(version: &str) -> Value {
        let args = json!({ "cli_version": version });
        check(&Args::new(&args).unwrap()).unwrap()
    }

    #[test]
    fn real_output_and_bare_versions() {
        let result = run("oak 0.110.0 (nightly 20261106.274889b0)\n");
        assert_eq!(result["ok"], true);
        assert_eq!(result["version"], "0.110.0");
        assert_eq!(result["capabilities"], json!(["desc_append", "ci_publish"]));

        let parsed = parse_cli_version("oak 0.110.0 (nightly 20261106.274889b0)").unwrap();
        assert_eq!(parsed.channel, "nightly");
        assert_eq!(parsed.build.as_deref(), Some("20261106.274889b0"));
        assert_eq!(parse_cli_version("v1.2.3-rc1").unwrap().channel, "stable");
    }

    #[test]
    fn too_old_and_unparseable() {
        let result = run("oak 0.105.2");
        assert_eq!(result["ok"], false);
        assert_eq!(result["capabilities"], json!(["desc_append"]));
        assert_eq!(run("oak 0.99.0")["capabilities"], json!([]));
        for bad in ["", "oak", "oak x.y.z", "oak 1.2", "oak 1.2.3.4"] {
            assert_eq!(run(bad)["ok"], false, "{bad}");
        }
        assert_eq!(run("garbage")["version"], Value::Null);
    }
}
