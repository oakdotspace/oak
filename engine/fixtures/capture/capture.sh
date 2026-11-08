#!/usr/bin/env bash
# Capture real `oak ... --json` output for `cli.parse`.
#
#   engine/fixtures/capture/capture.sh OUT_DIR [REAL_CHECKOUT]
#   engine/fixtures/capture/gen_fixtures.py OUT_DIR
#   UPDATE_ENGINE_FIXTURES=1 cargo test -p oakvcs-engine --test conformance
#
# Drives a throwaway repo against a throwaway loopback `oak serve`, with its own
# HOME so nothing touches real credentials. Each step writes
# OUT_DIR/<name>.{argv,exit,stdout,stderr}. `oak serve` has no CI, so with
# REAL_CHECKOUT (a checkout logged in to oak.space) the CI reads run there too;
# they are all read-only, and `ci publish` only ever with --dry-run.
set -uo pipefail

out="${1:?usage: capture.sh OUT_DIR [REAL_CHECKOUT]}"
real="${2:-}"
oak="${OAK_BIN:-$(command -v oak)}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
work="$(mktemp -d)"
real_home="$HOME"
export HOME="$work/home" OAK_NO_UPDATE_CHECK=1 NO_COLOR=1
mkdir -p "$HOME"
printf '%s\n' "$work" >"$out/.work"
port=$((20000 + RANDOM % 20000))
printf '%s\n' "$port" >"$out/.port"
"$oak" serve -d "$work/data" -p "$port" >"$work/serve.log" 2>&1 &
serve=$!
trap 'kill $serve 2>/dev/null; rm -rf "$work"' EXIT
for _ in $(seq 1 50); do
  curl -fsS -o /dev/null "http://127.0.0.1:$port/" 2>/dev/null && break
  curl -sS -o /dev/null "http://127.0.0.1:$port/" 2>/dev/null && break
  perl -e 'select(undef,undef,undef,0.1)'
done

cap() { # name cmd...
  local name="$1"
  shift
  printf '%s\n' "$*" >"$out/$name.argv"
  "$oak" "$@" >"$out/$name.stdout" 2>"$out/$name.stderr"
  echo $? >"$out/$name.exit"
}

cap version --version
if [ -n "$real" ]; then
  (
    cd "$real" || exit 1
    export HOME="$real_home"
    cap real_ci_status ci status --json
    cap real_ci_runs ci runs --json --limit 5
    run="$("$oak" ci runs --json --limit 1 | sed -nE 's/.*"id":([0-9]+).*/\1/p' | head -1)"
    cap real_ci_logs ci logs "${run:-1}" --json --summary
    cap real_ci_publish_preview ci publish -e production --workflow release --dry-run --json
  )
fi

mkdir "$work/a"
cd "$work/a" || exit 1
"$oak" init . >/dev/null 2>&1
printf 'one\ntwo\nthree\n' >a.txt
printf '\000\001bin' >b.uasset
mkdir -p Assets/Art
echo hi >Assets/Art/hero.png
echo meta >Assets/Art/hero.png.meta
cap status_clean_unborn status --json
cap commit_first commit --json --quiet
first="$("$oak" hash)"
cap push_not_linked push --json
cap push_first push -r "http://127.0.0.1:$port" --repo acme/game --json
cap status_clean status --json
cap info info --json
cap whoami_text whoami
cap whoami auth status --json
cap log log --json -n 5 -- a.txt

cap branch_create switch -c feature-x
echo four >>a.txt
echo new >new.txt
rm b.uasset
mv Assets/Art/hero.png Assets/Art/villain.png
cap status_dirty status --json
cap diff_dirty diff --json --hunks
cap diff_path diff --json --hunks -- a.txt
cap diff_max diff --json --hunks --max-bytes 10
cap switch_dirty switch main
cap desc_set desc "first line" --json
cap desc_append desc --append "second line" --json
printf "from stdin\n" | cap desc_append_stdin desc --append --file - --json
cap info_branch info --json
cap commit_branch commit --json --quiet -- .
second="$("$oak" hash)"
cap diff_commits diff --json --hunks "$first" "$second" -- a.txt
cap commit_nothing commit --json --quiet
cap push_branch push --json
cap branch_list branch list --json
cap fetch fetch
cap pull_uptodate pull --json
cap restore_clean restore -f -- a.txt
echo scratch >>a.txt
cap restore_dirty restore -f -- a.txt
cap merge_dry merge --dry-run --json
cap merge_ok merge --json
cap branch_switch_main switch main
cap pull_main pull --json

# A pull that conflicts, then its status.
"$oak" clone "http://127.0.0.1:$port/acme/game" "$work/b" >/dev/null 2>&1
cd "$work/b" || exit 1
"$oak" switch -c other >/dev/null 2>&1
echo theirs >a.txt
"$oak" commit --json --quiet >/dev/null 2>&1
"$oak" push --json >/dev/null 2>&1
"$oak" merge --json >/dev/null 2>&1
cd "$work/a" || exit 1
"$oak" switch -c mine >/dev/null 2>&1
echo ours >a.txt
"$oak" commit --json --quiet >/dev/null 2>&1
"$oak" push --json >/dev/null 2>&1
cap merge_conflict merge --json
cap pull_conflict pull --json
cap status_conflict status --json
cap conflict_status conflict status --json
cap pull_abort pull --abort

cap branch_close close --json mine
cap branch_close_missing close --json no-such-branch
cap branch_missing switch no-such-branch
cap ci_status ci status --json
cap ci_runs ci runs --json
cap ci_logs ci logs 1 --json
cap ci_publish_preview ci publish -e production --dry-run --json

# Failures with no server or outside a repo.
kill $serve 2>/dev/null
wait $serve 2>/dev/null
echo offline >>a.txt
cap push_offline push --json
cap fetch_offline fetch
cap pull_offline pull --json
cd "$work" || exit 1
cap status_not_repo status --json
cap info_not_repo info --json
exit 0
