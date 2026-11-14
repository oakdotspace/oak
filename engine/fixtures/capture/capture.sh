#!/usr/bin/env bash
# Capture real `oak ... --json` output for `cli.parse`.
#
#   OAK_REMOTE=URL OAK_API_KEY=KEY engine/fixtures/capture/capture.sh OUT_DIR [REAL_CHECKOUT]
#   engine/fixtures/capture/gen_fixtures.py OUT_DIR
#   UPDATE_ENGINE_FIXTURES=1 cargo test -p oakvcs-engine --test conformance
#
# Drives a throwaway repo against the Oak server at OAK_REMOTE (a local or
# staging server, never oak.space), with its own HOME so nothing touches real
# credentials. OAK_API_KEY must be a credential for that server that can
# create repos under OAK_CAPTURE_OWNER (default `acme`); the repo is
# OAK_CAPTURE_REPO (default `game`) and must not exist yet. Each step writes
# OUT_DIR/<name>.{argv,exit,stdout,stderr}. With REAL_CHECKOUT (a checkout
# logged in to oak.space) the CI reads run there too; they are all read-only,
# and `ci publish` only ever with --dry-run.
set -uo pipefail

out="${1:?usage: OAK_REMOTE=URL OAK_API_KEY=KEY capture.sh OUT_DIR [REAL_CHECKOUT]}"
real="${2:-}"
remote="${OAK_REMOTE:?set OAK_REMOTE to the server to capture against}"
remote="${remote%/}"
: "${OAK_API_KEY:?set OAK_API_KEY to a credential for $remote}"
owner="${OAK_CAPTURE_OWNER:-acme}"
repo="${OAK_CAPTURE_REPO:-game}"
# OAK_REMOTE would override every checkout's stored remote; the steps below
# pass the server explicitly instead.
unset OAK_REMOTE
oak="${OAK_BIN:-$(command -v oak)}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
work="$(mktemp -d)"
real_home="$HOME"
export HOME="$work/home" OAK_NO_UPDATE_CHECK=1 NO_COLOR=1
mkdir -p "$HOME"
printf '%s\n' "$work" >"$out/.work"
printf '%s\n' "$remote" >"$out/.remote"
# A loopback port with nothing listening, for the offline failures at the end.
dead="http://127.0.0.1:9"
printf '%s\n' "$dead" >"$out/.dead_remote"
trap 'rm -rf "$work"' EXIT
curl -sS -o /dev/null "$remote/" || {
  echo "capture.sh: no server answering at $remote" >&2
  exit 1
}

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
    unset OAK_API_KEY # the capture server's key; oak.space uses the stored login
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
cap push_first push -r "$remote" --repo "$owner/$repo" --json
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
"$oak" clone "$remote/$owner/$repo" "$work/b" >/dev/null 2>&1
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
echo offline >>a.txt
OAK_REMOTE="$dead" cap push_offline push --json
OAK_REMOTE="$dead" cap fetch_offline fetch
OAK_REMOTE="$dead" cap pull_offline pull --json
cd "$work" || exit 1
cap status_not_repo status --json
cap info_not_repo info --json
exit 0
