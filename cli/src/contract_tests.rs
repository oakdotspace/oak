//! Mechanical enforcement of the machine contract against the real clap
//! definition (`Cli`), which lives in the binary crate.
//!
//! * every recommendation the error mapping can produce parses;
//! * every complete `"oak ..."` command literal in the CLI source parses;
//! * the command surface (which leaves accept `--json`, and why the others
//!   do not) matches the reviewed golden file the process-level harness
//!   (`cli/tests/it/contract_surface.rs`) walks.

use super::*;
use clap::CommandFactory;
use oak_cli::contract::{self, NextCommand};
use std::collections::BTreeMap;

fn parses(line: &str) -> Result<(), String> {
    recommended_command_parses(line)
}

#[test]
fn every_next_command_constructor_renders_a_command_clap_accepts() {
    let failures: Vec<String> = NextCommand::samples()
        .iter()
        .filter_map(|command| parses(&command.render()).err())
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_error_mapping_recommendation_is_a_command_clap_accepts() {
    let mut checked = 0;
    let mut failures = Vec::new();
    for error in contract::sample_errors() {
        // Both renderings a caller can receive: the bare contract and the
        // `--json` envelope built from it.
        let envelope = output::JsonErrorEnvelope::from_error_in(
            &error,
            &contract::AdviceContext::unknown(),
            None,
        );
        let contract = contract::classify(&error);
        assert_eq!(
            envelope.error.recommended_next_commands,
            contract.rendered_next_commands()
        );
        for command in &contract.next {
            // "Run the same command again" echoes this process's own argv —
            // here the test runner's, in production one clap already parsed.
            if command.kind() == contract::NextCommandKind::SameInvocation {
                continue;
            }
            checked += 1;
            if let Err(failure) = parses(&command.render()) {
                failures.push(format!("{:?}: {failure}", error.kind()));
            }
        }
    }
    assert!(checked > 40, "only {checked} recommendations were checked");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The run-time sweep: in debug builds `output::print_json` passes every
/// command an emitted document recommends through `recommended_command_
/// parses` (installed by `main`) and panics on one that does not parse — so
/// every integration test that runs the binary checks the advice it
/// produced, however the string was assembled (`format!`, multi-line
/// literals, carried fields). This pins the two halves of that check.
#[test]
fn emitted_documents_are_swept_for_unrunnable_commands() {
    let document = serde_json::json!({
        "recommended_next_commands": ["oak status --json", "not an oak command"],
        "error": {
            "recommended_next_commands": ["oak fetch"],
            "finish": {"retry_command": "oak finish --json", "manual_recovery_commands": ["oak push --json"]},
        },
        "changed_files_page": {"next_page_command": "oak diff --json --changed-files-offset 20"},
        "details_command": "oak ci logs 7 --summary --json",
        "note": "oak statuss --json",
        "recommended_action_detail": {"kind": "push", "command": "oak push --json"},
        "jobs": [{"steps": [{"command": "oak not-advice"}]}],
        "set_command_template": "oak retention set DIR --keep N",
    });
    let mut found = output::recommended_commands_in(&document);
    found.sort();
    assert_eq!(
        found,
        vec![
            "oak ci logs 7 --summary --json",
            "oak diff --json --changed-files-offset 20",
            "oak fetch",
            "oak finish --json",
            "oak push --json",
            "oak push --json",
            "oak status --json",
        ]
    );
    for command in &found {
        assert_eq!(recommended_command_parses(command), Ok(()), "{command}");
    }
    assert!(recommended_command_parses("oak statuss --json").is_err());
    assert!(recommended_command_parses("oak mount status --json --compact").is_err());
    assert!(recommended_command_parses("oak retention set DIR --keep N").is_err());
}

/// Complete command literals in the CLI source that are deliberately not
/// runnable as written, with the reason. Keep this short.
const NON_COMMAND_LITERALS: &[(&str, &str)] = &[
    (
        "oak tree inspect --json",
        "prefix: `larger_budget_command` appends the pinned commit and budgets",
    ),
    (
        "oak watch hook",
        "prefix: the installed hook command is this plus the event name",
    ),
    (
        "oak mount finish",
        "command name used in `read_desc_input` messages, not a recommendation",
    ),
    (
        "oak watch runs in regular checkouts; a mount publishes through `oak mount finish` / `oak finish` instead",
        "error message",
    ),
];

/// A source-level sweep: every string literal that is, in full, an `oak ...`
/// command line with no format placeholders (the shape of an entry in a
/// `recommended_next_commands` / `next_commands` list) must parse. Literals built with `format!` are not
/// covered here; they are converted to [`NextCommand`] constructors over
/// time (the count is reported by `dynamic_command_literal_inventory`).
#[test]
fn every_static_command_literal_in_the_cli_source_parses() {
    let mut checked = 0;
    let mut failures = Vec::new();
    for (file, line_number, literal) in static_command_literals() {
        if NON_COMMAND_LITERALS
            .iter()
            .any(|(text, _)| *text == literal)
        {
            continue;
        }
        checked += 1;
        if let Err(failure) = parses(&literal) {
            failures.push(format!("{file}:{line_number}: {failure}"));
        }
    }
    assert!(
        checked > 100,
        "only {checked} literals were found; the sweep is broken"
    );
    eprintln!("static command literals checked: {checked}");
    assert!(
        failures.is_empty(),
        "{} command literal(s) do not parse:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn cli_source_files() -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    files.sort();
    files
}

/// Non-test source lines of one file, with 1-based line numbers. Every
/// `#[cfg(test)]` item is skipped — wherever it sits in the file — by
/// dropping the lines from the attribute to the item's closing brace at the
/// same indentation (or to the `;` of an out-of-line `mod tests;`).
fn production_lines(source: &str) -> Vec<(usize, &str)> {
    let mut kept = Vec::new();
    let mut lines = source.lines().enumerate().peekable();
    while let Some((index, line)) = lines.next() {
        if line.trim() != "#[cfg(test)]" {
            kept.push((index + 1, line));
            continue;
        }
        let indent = &line[..line.len() - line.trim_start().len()];
        let closing = format!("{indent}}}");
        // Skip the item's header: stops at the line that opens its body or
        // ends it outright (`mod tests;`, `use ...;`).
        let mut opened = false;
        for (_, header) in lines.by_ref() {
            if header.trim_end().ends_with('{') {
                opened = true;
                break;
            }
            if header.trim_end().ends_with(';') {
                break;
            }
        }
        if opened {
            for (_, body) in lines.by_ref() {
                if body == closing || body.starts_with(&format!("{closing} ")) {
                    break;
                }
            }
        }
    }
    kept
}

#[test]
fn production_lines_skips_test_items_anywhere_in_a_file() {
    let source = "fn a() {}\n#[cfg(test)]\nmod early {\n    fn hidden() {}\n}\nfn b() {}\n    #[cfg(test)]\n    fn helper() {\n        hidden();\n    }\nfn c() {}\n#[cfg(test)]\nmod out_of_line;\nfn d() {}\n";
    let kept: Vec<&str> = production_lines(source)
        .into_iter()
        .map(|(_, line)| line)
        .collect();
    assert_eq!(
        kept,
        vec!["fn a() {}", "fn b() {}", "fn c() {}", "fn d() {}"]
    );
}

/// `(file, line, literal)` for every `"oak <lowercase verb>..."` literal that
/// is a whole string with no `{`/`}`/escape and does not read as prose.
fn static_command_literals() -> Vec<(String, usize, String)> {
    let mut found = Vec::new();
    for path in cli_source_files() {
        if path.ends_with("contract_tests.rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let file = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap()
            .display()
            .to_string();
        for (line_number, line) in production_lines(&source) {
            let mut rest = line;
            while let Some(start) = rest.find("\"oak ") {
                let after = &rest[start + 1..];
                let Some(end) = after.find('"') else { break };
                let literal = &after[..end];
                let tail = &after[end + 1..];
                rest = tail;
                let verb_ok = literal[4..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase());
                // A trailing space marks a prefix that code completes or
                // matches against (`"oak start "`), not a command line.
                if verb_ok
                    && !literal.ends_with(' ')
                    && !literal.contains(['{', '}', '\\'])
                    && !reads_as_prose(literal)
                {
                    found.push((file.clone(), line_number, literal.to_string()));
                }
            }
        }
    }
    found
}

/// Error messages also start with a command name ("oak desc --append needs
/// non-empty text"). A command line never has one of these as a bare word,
/// nor a word ending in a colon or closing parenthesis.
fn reads_as_prose(literal: &str) -> bool {
    const PROSE_WORDS: &[&str] = &[
        "requires",
        "needs",
        "is",
        "currently",
        "has",
        "cannot",
        "must",
        "does",
        "takes",
        "accepts",
        "only",
        "supports",
        "was",
        "will",
        "can",
    ];
    literal
        .split_whitespace()
        .skip(1)
        .any(|word| PROSE_WORDS.contains(&word) || word.ends_with(':') || word.ends_with(')'))
}

/// Not an assertion: an inventory of command lines still assembled with
/// `format!` (which the static sweep above cannot see), printed so the
/// remaining conversion work to `NextCommand` stays visible.
#[test]
fn dynamic_command_literal_inventory() {
    let mut dynamic = 0;
    for path in cli_source_files() {
        let source = std::fs::read_to_string(&path).unwrap();
        for (_, line) in production_lines(&source) {
            let mut rest = line;
            while let Some(start) = rest.find("\"oak ") {
                let after = &rest[start + 1..];
                let Some(end) = after.find('"') else { break };
                if after[..end].contains('{') {
                    dynamic += 1;
                }
                rest = &after[end + 1..];
            }
        }
    }
    // Informational only. These strings are checked where it counts — at
    // run time, when a document that recommends one is emitted (see
    // `emitted_documents_are_swept_for_unrunnable_commands`) — so a fixed
    // ceiling here would only make unrelated work on other branches fail.
    eprintln!("format!-built `\"oak ...{{}}\"` literals in cli/src: {dynamic}");
}

/// Every invocation leaves through `main`'s epilogue. The only code allowed
/// to end the process directly is listed here, with the reason; a new
/// `process::exit` / `exit_process` call anywhere else fails this test (use
/// `output::set_exit_status` for a data exit code, or return the error).
#[test]
fn no_direct_process_exit_outside_the_epilogue() {
    // (file, calls, why)
    const ALLOWED: &[(&str, usize, &str)] = &[
        (
            "src/output.rs",
            3,
            "`exit_process` itself: the checked exit the epilogue uses",
        ),
        (
            "src/main.rs",
            6,
            "the epilogue (`exit_with_error`, `exit_with_usage_error`, the data-code exit) \
             and help/version, which exit 0 before any command exists",
        ),
        (
            "src/commands/feedback.rs",
            2,
            "interactive `$EDITOR` flow aborted by the user (needs a terminal; never JSON)",
        ),
        (
            "src/commands/mount/fskit/broker.rs",
            1,
            "hidden internal broker process (`oak mount __fskit-broker`), not a user command",
        ),
    ];
    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    for path in cli_source_files() {
        if path.ends_with("contract_tests.rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        // Any way of naming the function counts: a call through the path,
        // an import of it (`use std::process::exit`, `process::{exit, ..}`)
        // — which is what makes a bare `exit(3)` possible — and the checked
        // wrapper.
        let calls = production_lines(&source)
            .into_iter()
            .filter(|(_, line)| !line.trim_start().starts_with("//"))
            .map(|(_, line)| {
                let imports_exit = line.contains("process::{")
                    && line
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|word| word == "exit");
                line.matches("process::exit").count()
                    + line.matches("exit_process(").count()
                    + usize::from(imports_exit)
            })
            .sum::<usize>();
        if calls > 0 {
            let file = path
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .unwrap()
                .display()
                .to_string();
            found.insert(file, calls);
        }
    }
    let allowed: BTreeMap<String, usize> = ALLOWED
        .iter()
        .map(|(file, calls, _)| (file.to_string(), *calls))
        .collect();
    assert_eq!(
        found, allowed,
        "direct process exits changed; route new ones through the epilogue"
    );
}

/// The contract matches are exhaustive on purpose: a wildcard arm would let
/// a new error variant (or blocker, or refusal) fall into someone else's
/// code, exit and advice without anyone deciding. The compiler cannot see a
/// wildcard as a mistake, so the source is checked: none of these functions
/// may contain a `_ =>` arm or a bare catch-all binding.
#[test]
fn contract_matches_have_no_wildcard_arm() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let checks: &[(&str, &str)] = &[
        ("src/contract.rs", "pub fn classify_in("),
        ("src/contract.rs", "fn finish_blocker_exit("),
        ("src/contract.rs", "pub fn kind(&self) -> NextCommandKind {"),
        (
            "../core/src/error.rs",
            "pub fn kind(&self) -> OakErrorKind {",
        ),
    ];
    for (file, signature) in checks {
        let source = std::fs::read_to_string(manifest.join(file)).unwrap();
        let body = function_body(&source, signature)
            .unwrap_or_else(|| panic!("{file}: `{signature}` not found"));
        assert!(
            body.lines().count() > 5,
            "{file}: `{signature}` body not captured"
        );
        for (offset, line) in body.lines().enumerate() {
            let code = line.split("//").next().unwrap_or_default().trim();
            let catch_all = code.starts_with("_ =>")
                || code.starts_with("_ if ")
                || code.contains("| _ =>")
                || is_bare_binding_arm(code);
            assert!(
                !catch_all,
                "{file}: `{signature}` has a catch-all arm at body line {}: `{code}` — \
                 name every variant instead",
                offset + 1
            );
        }
    }
}

/// `name =>` / `name if ... =>`: an arm that binds anything.
fn is_bare_binding_arm(code: &str) -> bool {
    let Some((pattern, _)) = code.split_once("=>") else {
        return false;
    };
    let pattern = pattern.split(" if ").next().unwrap_or_default().trim();
    !pattern.is_empty()
        && pattern
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
}

/// The text of the function starting at `signature`, through the closing
/// brace at the signature's indentation.
fn function_body<'a>(source: &'a str, signature: &str) -> Option<&'a str> {
    let start = source.find(signature)?;
    let line_start = source[..start].rfind('\n').map_or(0, |at| at + 1);
    let indent = &source[line_start..start];
    let indent = &indent[..indent.len() - indent.trim_start().len()];
    let closing = format!("\n{indent}}}\n");
    let end = source[start..].find(&closing)?;
    Some(&source[start..start + end])
}

#[test]
fn the_wildcard_check_catches_wildcards() {
    let with_wildcard =
        "pub fn f(x: E) -> u8 {\n    match x {\n        E::A => 1,\n        _ => 2,\n    }\n}\n";
    let body = function_body(with_wildcard, "pub fn f(").unwrap();
    assert!(body.lines().any(|line| line.trim().starts_with("_ =>")));
    assert!(is_bare_binding_arm("other => 2,"));
    assert!(is_bare_binding_arm("other if other.x() => 2,"));
    assert!(!is_bare_binding_arm(
        "OakError::Io(_) => contract(Code::IoError, Exit::Failure),"
    ));
    assert!(!is_bare_binding_arm("} => contract(*code, Exit::Failure)"));
}

// ---------------------------------------------------------------------------
// Command surface
// ---------------------------------------------------------------------------

const SURFACE_GOLDEN: &str = "tests/it/contract_surface.tsv";

/// The clap tree with global arguments propagated to the subcommands that
/// inherit them (`oak team --json` reaches `oak team list`).
fn built_command() -> clap::Command {
    let mut command = Cli::command();
    command.build();
    command
}

struct Leaf {
    path: String,
    accepts_json: bool,
    hidden: bool,
}

/// Every runnable command: nodes without subcommands, and nodes whose
/// subcommand is optional (`oak branch`, `oak sparse`).
fn leaves() -> Vec<Leaf> {
    fn walk(command: &clap::Command, path: &mut Vec<String>, hidden: bool, out: &mut Vec<Leaf>) {
        let hidden = hidden || command.is_hide_set();
        let has_subcommands = command.get_subcommands().next().is_some();
        let runnable = !has_subcommands
            || !(command.is_subcommand_required_set() || command.is_arg_required_else_help_set());
        if runnable && !path.is_empty() {
            out.push(Leaf {
                path: path.join(" "),
                accepts_json: command
                    .get_arguments()
                    .any(|arg| arg.get_long() == Some("json")),
                hidden,
            });
        }
        for sub in command.get_subcommands() {
            if sub.get_name() == "help" {
                continue;
            }
            path.push(sub.get_name().to_string());
            walk(sub, path, hidden, out);
            path.pop();
        }
    }
    let mut out = Vec::new();
    walk(&built_command(), &mut Vec::new(), false, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// JSON mode is read from the argument id `json` as a boolean
/// (`invocation_requests_json`), so every `--json` flag must be exactly that.
#[test]
fn every_json_flag_is_a_boolean_named_json() {
    fn check(command: &clap::Command, path: &str) {
        for arg in command.get_arguments() {
            if arg.get_long() == Some("json") {
                assert_eq!(
                    arg.get_id().as_str(),
                    "json",
                    "{path}: --json has another id"
                );
                assert!(
                    matches!(arg.get_action(), clap::ArgAction::SetTrue),
                    "{path}: --json is not a plain boolean flag"
                );
            } else {
                assert_ne!(
                    arg.get_id().as_str(),
                    "json",
                    "{path}: id `json` without --json"
                );
            }
        }
        for sub in command.get_subcommands() {
            check(sub, &format!("{path} {}", sub.get_name()));
        }
    }
    check(&built_command(), "oak");
}

#[test]
fn json_mode_is_read_from_any_level_of_the_command_path() {
    let json_mode = |args: &[&str]| {
        invocation_requests_json(&Cli::command().try_get_matches_from(args).unwrap())
    };
    assert!(json_mode(&["oak", "status", "--json"]));
    assert!(!json_mode(&["oak", "status"]));
    assert!(json_mode(&["oak", "branch", "list", "--json"]));
    assert!(json_mode(&["oak", "sparse", "--json", "add", "src"]));
    assert!(json_mode(&["oak", "sparse", "add", "src", "--json"]));
    assert!(json_mode(&[
        "oak",
        "agent",
        "publication",
        "forget",
        "op",
        "--json"
    ]));
    // A literal `--json` after `--` is a value, not the flag.
    assert!(!json_mode(&["oak", "desc", "--", "--json"]));
    let argv = |args: &[&str]| -> Vec<String> { args.iter().map(|a| a.to_string()).collect() };
    assert!(argv_requests_json(&argv(&[
        "oak", "status", "--bogus", "--json"
    ])));
    assert!(!argv_requests_json(&argv(&[
        "oak", "desc", "--bogus", "--", "--json"
    ])));
    assert!(!argv_requests_json(&argv(&["oak", "status", "--bogus"])));
}

/// The golden file is the reviewed statement of the command surface: one
/// line per runnable command, `json` or `exempt` plus the reason it has no
/// `--json`. This test fails when a command is added, removed, gains or
/// loses `--json` without the file changing — so "this command has no JSON
/// mode" is always a decision someone wrote down. Regenerate (keeping the
/// existing reasons) with `UPDATE_CONTRACT_SURFACE=1 cargo test -p
/// oakvcs-cli --bin oak contract_surface_matches_the_golden_file`.
#[test]
fn contract_surface_matches_the_golden_file() {
    let golden_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SURFACE_GOLDEN);
    let golden = std::fs::read_to_string(&golden_path).unwrap_or_default();
    let mut reasons: BTreeMap<String, String> = BTreeMap::new();
    for line in golden
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
    {
        let mut fields = line.splitn(3, '\t');
        let path = fields.next().unwrap_or_default().to_string();
        let _mode = fields.next();
        reasons.insert(path, fields.next().unwrap_or_default().to_string());
    }
    let mut expected = String::from(
        "# The oak command surface for the machine contract. One line per runnable command:\n\
         # <command path> TAB <json|exempt> TAB <why an exempt command has no --json>.\n\
         # Checked by `contract_surface_matches_the_golden_file` (cli/src/contract_tests.rs);\n\
         # walked by cli/tests/it/contract_surface.rs. Hidden commands are marked `hidden`.\n",
    );
    let mut missing_reasons = Vec::new();
    for leaf in leaves() {
        let reason = if leaf.accepts_json {
            String::new()
        } else {
            let reason = reasons.get(&leaf.path).cloned().unwrap_or_default();
            if reason.trim().is_empty() {
                missing_reasons.push(leaf.path.clone());
            }
            reason
        };
        let mode = match (leaf.accepts_json, leaf.hidden) {
            (true, false) => "json",
            (true, true) => "json-hidden",
            (false, false) => "exempt",
            (false, true) => "exempt-hidden",
        };
        expected.push_str(&format!("{}\t{mode}\t{reason}\n", leaf.path));
    }
    if std::env::var_os("UPDATE_CONTRACT_SURFACE").is_some() {
        std::fs::write(&golden_path, &expected).unwrap();
    }
    assert_eq!(
        golden, expected,
        "the command surface changed; review and regenerate {SURFACE_GOLDEN} (see this test's docs)"
    );
    assert!(
        missing_reasons.is_empty(),
        "commands without --json need a written reason in {SURFACE_GOLDEN}: {missing_reasons:?}"
    );
}
