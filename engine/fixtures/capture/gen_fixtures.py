#!/usr/bin/env python3
"""Turn a capture.sh directory into engine/fixtures/conformance/*.json requests.

Each fixture file is {"name", "cases": [{"name", "source", "request", "response"}]}.
`source` is "captured" for real CLI output, "derived" where the case can't be
produced here and was written from the CLI source, and "spec" for pure ops.
This script writes requests with empty responses; the conformance test fills
them in with UPDATE_ENGINE_FIXTURES=1, and the diff is what you review.
"""
import json
import pathlib
import sys

cap = pathlib.Path(sys.argv[1])
out = pathlib.Path(__file__).resolve().parent.parent / "conformance"
work = (cap / ".work").read_text().strip()
remote = (cap / ".remote").read_text().strip()
dead_remote = (cap / ".dead_remote").read_text().strip()
# Fixtures name one stable server, whichever one the capture ran against.
FIXTURE_REMOTE = "http://127.0.0.1:8765"


def host(url):
    return url.split("://", 1)[-1]


def scrub(text):
    for old, new in [
        ("/private" + work, "/tmp/oak-capture"),
        (work, "/tmp/oak-capture"),
        (remote, FIXTURE_REMOTE),
        (dead_remote, FIXTURE_REMOTE),
        (host(remote), host(FIXTURE_REMOTE)),
        (host(dead_remote), host(FIXTURE_REMOTE)),
        ("zdgeier-", "dev-"),
    ]:
        text = text.replace(old, new)
    return text


def read(name, suffix):
    path = cap / f"{name}.{suffix}"
    return scrub(path.read_text()) if path.exists() else ""


def only_main_runs(text):
    """Other people's in-flight branches have no place in a fixture."""
    doc = json.loads(text)
    doc["runs"] = [run for run in doc["runs"] if run.get("branch") == "main"][:2]
    return json.dumps(doc)


CAPTURED = [
    ("status", "status_clean_unborn"), ("status", "status_clean"), ("status", "status_dirty"),
    ("status", "status_conflict"), ("status", "status_not_repo"),
    ("info", "info"), ("info", "info_branch"), ("info", "info_not_repo"),
    ("desc_get", "info_branch"),
    ("diff", "diff_dirty"), ("diff", "diff_path"), ("diff", "diff_max"), ("diff", "diff_commits"),
    ("log", "log"), ("branch_list", "branch_list"),
    ("branch_create", "branch_create"),
    ("branch_switch", "switch_dirty"), ("branch_switch", "branch_missing"),
    ("branch_switch", "branch_switch_main"),
    ("branch_close", "branch_close"), ("branch_close", "branch_close_missing"),
    ("commit", "commit_first"), ("commit", "commit_branch"), ("commit", "commit_nothing"),
    ("desc_append", "desc_append"), ("desc_append", "desc_append_stdin"),
    ("fetch", "fetch"), ("fetch", "fetch_offline"),
    ("pull", "pull_uptodate"), ("pull", "pull_main"), ("pull", "pull_conflict"),
    ("pull", "pull_offline"), ("pull", "pull_abort"),
    ("push", "push_branch"), ("push", "push_not_linked"), ("push", "push_offline"),
    ("merge", "merge_ok"), ("merge", "merge_conflict"),
    ("restore", "restore_clean"), ("restore", "restore_dirty"),
    ("whoami", "whoami"), ("whoami", "whoami_text"),
    ("ci_status", "ci_status"), ("ci_runs", "ci_runs"), ("ci_logs", "ci_logs"),
    ("ci_publish_preview", "ci_publish_preview"),
    ("conflict_status", "conflict_status"), ("version", "version"),
    ("ci_status", "real_ci_status"), ("ci_runs", "real_ci_runs"),
    ("ci_logs", "real_ci_logs"), ("ci_publish_preview", "real_ci_publish_preview"),
]

DERIVED = [
    ("merge", "merge_ci_gate", 6, "",
     "error: Server error: Merge failed: HTTP 412 Precondition Failed\n"
     "CI gates this merge \u2014 check it with `oak ci status`, or run `oak merge --wait` "
     "to merge automatically when CI concludes.\n"),
    ("status", "repo_locked", 3,
     '{"schema_version":1,"error":{"code":"repo_locked","message":"Repository is locked",'
     '"conflict_paths":[],"recommended_next_commands":["oak status --json"]}}', ""),
    ("push", "auth_required", 6,
     '{"schema_version":1,"error":{"code":"auth_required","message":"Authentication required",'
     '"conflict_paths":[],"recommended_next_commands":["oak login --remote=https://oak.space"]}}',
     ""),
    ("desc_append", "description_stale", 5,
     '{"schema_version":1,"error":{"code":"description_stale","message":"Refusing to append",'
     '"conflict_paths":[],"recommended_next_commands":["oak pull --branch-only --json"]}}', ""),
    ("ci_publish", "publish_authorized", 0,
     '{"schema_version":1,"outcome":"authorized","preview":{"environment":"production"'
     ',"branch":"main","commit":"' + "a" * 64 + '","workflow":"release","ready":true,'
     '"can_authorize":true,"problems":[]},"authorization":{"authorization_id":"auth-1",'
     '"run_id":42,"run_url":"https://oak.space/oak/oak/ci/runs/42"}}', ""),
    ("ci_publish", "publish_not_ready", 1,
     '{"schema_version":1,"outcome":"not_ready","preview":{"environment":"production",'
     '"ready":false,"problems":["no runner online"]}}', ""),
]


def op(name, op_name, args, source="spec"):
    return {"name": name, "source": source,
            "request": {"v": 1, "op": op_name, "args": args}, "response": None}


def parse_case(action, name, exit_code, stdout, stderr, source):
    return op(f"{action}/{name}", "cli.parse",
              {"action": action, "exit_code": exit_code, "stdout": stdout, "stderr": stderr},
              source)


cases = []
for action, name in CAPTURED:
    exit_path = cap / f"{name}.exit"
    if not exit_path.exists():
        continue
    stdout = read(name, "stdout")
    if name == "real_ci_runs":
        stdout = only_main_runs(stdout)
    cases.append(parse_case(action, name, int(exit_path.read_text()), stdout, read(name, "stderr"),
                            "captured"))
for action, name, exit_code, stdout, stderr in DERIVED:
    cases.append(parse_case(action, name, exit_code, stdout, stderr, "derived"))

HASH = "b" * 64
plans = [
    ("status", {}), ("info", {}), ("version", {}), ("conflict_status", {}),
    ("diff", {"paths": ["Assets/a.png"], "max_bytes": 65536}),
    ("diff", {"from": "abc123", "to": "def456", "paths": ["a.txt"]}), ("diff", {"to": "def456"}),
    ("log", {"limit": 10}), ("branch_list", {}), ("desc_get", {}),
    ("branch_create", {"name": "hero-pass"}), ("branch_switch", {"name": "main"}),
    ("branch_close", {"name": "hero-pass"}), ("commit", {"paths": ["a b.txt", "--odd"]}),
    ("desc_append", {"message": "second line"}), ("fetch", {}), ("pull", {}),
    ("pull", {"continue": True}), ("push", {}), ("merge", {"wait_minutes": 5}),
    ("restore", {"paths": ["a.txt"]}), ("whoami", {}), ("ci_status", {}), ("ci_runs", {"limit": 5}),
    ("ci_logs", {"run": 42, "summary": True}),
    ("ci_publish_preview", {"environment": "production", "workflow": "release"}),
    ("ci_publish", {"environment": "production", "workflow": "release", "commit": HASH}),
    ("ci_publish", {"environment": "production", "commit": HASH, "republish": True}),
    ("restore", {}), ("commit", {"paths": ["../escape"]}), ("rebase", {}),
]
plan_cases = [
    op(f"{i:02}-{action}", "cli.plan", {"action": action, "params": params})
    for i, (action, params) in enumerate(plans)
]

ops = [
    op("detect-missing-path", "engine.detect", {"path": "/nonexistent/oak-engine-fixture"}),
    op("template-godot", "ignore.template", {"engine": "godot"}),
    op("template-unity", "ignore.template", {"engine": "unity"}),
    op("template-unreal", "ignore.template", {"engine": "unreal"}),
    op("template-unknown", "ignore.template", {"engine": "cryengine"}),
    op("classify-godot", "assets.classify", {"engine": "godot", "paths": [
        "icon.png", "icon.png.import", "player.gd", "player.gd.uid", "level.tscn", "level.scn"]}),
    op("classify-unity", "assets.classify", {"engine": "unity", "paths": [
        "Assets/a.png", "Assets/a.png.meta", "Assets/Main.unity", "Assets/P.cs"]}),
    op("classify-unity-force-text", "assets.classify", {
        "engine": "unity", "unity_force_text": True, "paths": ["Assets/Main.unity"]}),
    op("classify-unreal", "assets.classify", {"engine": "unreal", "paths": [
        "Content/Hero.uasset", "Content/Map.umap", "Source/Game.cpp"]}),
    op("companions-godot", "assets.companions", {"engine": "godot", "paths": ["a.png", "b.gd"]}),
    op("companions-unity", "assets.companions", {
        "engine": "unity", "paths": ["Assets/a.png", "Assets/Art/", "ProjectSettings/x.asset"]}),
    op("companions-reverse", "assets.companions", {"engine": "unity", "paths": ["Assets/a.png.meta"]}),
    op("companions-unreal", "assets.companions", {"engine": "unreal", "paths": ["Content/a.uasset"]}),
    op("rollup", "status.rollup", {"changes": [
        {"path": "Assets/Art/a.png", "status": "added"},
        {"path": "Assets/b.cs", "status": "modified"},
        {"path": "New/c.png", "status": "renamed", "old_path": "Old/c.png"}]}),
    op("diff", "diff.text", {"old": "a\nb\nc\n", "new": "a\nB\nc\nd\n"}),
    op("diff-equal", "diff.text", {"old": "a\n", "new": "a\n"}),
    op("merge-clean", "merge.text3", {"base": "a\nb\nc\n", "ours": "A\nb\nc\n", "theirs": "a\nb\nC\n"}),
    op("merge-conflict", "merge.text3", {"base": "a\n", "ours": "b\n", "theirs": "c\n"}),
    op("narrative", "narrative.append", {"existing": "first", "message": "second"}),
    op("narrative-empty", "narrative.append", {"existing": "", "message": "first"}),
    op("candidates-darwin", "binary.candidates", {
        "platform": "darwin", "home": "/Users/ada", "override": "", "path_env": "/usr/bin:/bin"}),
    op("candidates-windows", "binary.candidates", {
        "platform": "windows", "home": "C:\\Users\\ada", "override": "D:\\oak.exe",
        "path_env": "C:\\tools"}),
    op("version-ok", "version.check", {"cli_version": "oak 0.110.0 (nightly 20261106.274889b0)"}),
    op("version-old", "version.check", {"cli_version": "oak 0.105.0"}),
    op("unknown-op", "no.such.op", {}),
    {"name": "bad-version", "source": "spec", "request": {"v": 99, "op": "version.check", "args": {}},
     "response": None},
]

for name, items in [("cli_parse", cases), ("cli_plan", plan_cases), ("ops", ops)]:
    (out / f"{name}.json").write_text(
        json.dumps({"name": name, "cases": items}, indent=1, ensure_ascii=False) + "\n")
print(len(cases), "parse,", len(plan_cases), "plan,", len(ops), "op cases")
