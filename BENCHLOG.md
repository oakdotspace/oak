# Oak Benchmark Log

Benchmark repo: `/Users/mrmrs/o/oak-benchmarks`
Oak repo: `/Users/mrmrs/o/oak`
Baseline run: `results/devloop/20260610T093504Z/`
Final accepted run: `results/devloop/20260610T095004Z/`

## Baseline

- Command: `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak --lanes core,workflow,contention --runs 2 --noise-floor-runs 3`
- Verdict: REGRESSED.
- Main regressions:
  - `switch -c` branch creation emitted 129-143 bytes vs 62 bytes installed Oak.
  - `wide_config_refactor/vcs.branch`: 51.4 ms local vs 5.7 ms installed.
  - `large_asset_manifest/vcs.branch`: 24.3 ms local vs 6.0 ms installed.
  - `large_asset_manifest/vcs.diff`: about 67,110,439 bytes and about 5,011 estimated tokens local.

## Experiment 1: Compact `switch -c` Success Output

- Hypothesis: The branch-create output regression is self-inflicted by extra success-detail lines. A one-line confirmation should reduce agent-ingested bytes and remove the efficiency gate failure.
- Files changed: `cli/src/commands/switch.rs`.
- Benchmark command: `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak --lanes core --runs 1 --noise-floor-runs 2`
- Result: `results/devloop/20260610T093707Z/`, REGRESSED only on branch latency. Branch output dropped to 49 bytes local vs 62 bytes installed, with no output-byte regression.
- Verdict: kept. Output win was real; latency needed a separate fix.
- Next idea: remove unnecessary work from the branch-preserve path.

## Experiment 2: Skip Preserve-Worktree Rehash

- Hypothesis: When `switch -c` preserves the worktree, branch metadata changes but file contents do not. Rehashing the whole tree during branch creation is redundant; the next status/diff/commit can validate paths through the normal stat-cache guard.
- Files changed: `cli/src/commands/switch.rs`.
- Benchmark command: `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak --lanes core --runs 1 --noise-floor-runs 2`
- Result: `results/devloop/20260610T093816Z/`, PASS. Core branch latency regressions disappeared.
- Primary check: `results/devloop/20260610T093824Z/`, still REGRESSED on `wide_config_refactor/vcs.branch` because `switch -c` still ran a full dirty-status scan before discovering there was no local main head.
- Verdict: kept, but incomplete.
- Next idea: defer the dirty-status scan when there is no main head to materialize.

## Experiment 3: Defer Dirty Scan When Main Is Absent

- Hypothesis: If no local/remote main head exists, `switch -c` will preserve the worktree regardless of dirtiness, so it can skip the pre-branch status scan. If a remote or local main head exists, preserve the original pre-fetch dirty check to avoid wiping work.
- Files changed: `cli/src/commands/switch.rs`.
- Benchmark commands:
  - `cargo test -p oakvcs-cli test_switch_create --test integration`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak --lanes core --runs 1 --noise-floor-runs 2`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak --lanes core,workflow,contention --runs 2 --noise-floor-runs 3`
- Result:
  - First version failed `test_switch_create_without_name_fetches_remote_main_when_not_recently_checked`; fixed by doing the dirty check before remote fetch when a remote or local main head exists.
  - Switch tests passed after correction.
  - Primary PASS at `results/devloop/20260610T094118Z/`.
  - `wide_config_refactor/vcs.branch` improved from about 51 ms to about 8-9 ms local.
- Verdict: kept.
- Next idea: attack high-output diffs.

## Experiment 4: Omit Huge Text Diff Hunks

- Hypothesis: Oak's NUL-only binary heuristic lets large non-NUL asset fixtures render as giant one-line text diffs. Capping text diff rendering at 1 MiB should preserve path visibility while cutting output bytes and tokens.
- Files changed: `cli/src/commands/diff.rs`, `cli/tests/integration.rs`.
- Benchmark commands:
  - `cargo test -p oakvcs-cli test_diff_omits_large_text_hunks_but_keeps_path --test integration`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak --lanes core --runs 1 --noise-floor-runs 2`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak --lanes core,workflow,contention --runs 2 --noise-floor-runs 3`
- Result:
  - New test passed.
  - Fast core PASS at `results/devloop/20260610T094529Z/`.
  - Core `diff.dirty` output for binary-heavy scenarios dropped from about 16,777,508 bytes to 185-370 bytes.
  - Workflow `large_asset_manifest/vcs.diff` dropped from about 67,110,439 bytes and 5,011 estimated tokens to 1,727 bytes and 443 estimated tokens.
  - Primary `results/devloop/20260610T094538Z/` failed on unrelated `contention_shared_checkout_w8` throughput, which does not exercise diff.
- Verdict: kept, pending contention fix.
- Next idea: confirm and address contention median.

## Experiment 5: Bounded Commit Lock Wait

- Hypothesis: Under shared-checkout contention, `oak commit` exits immediately on `.oak/wdlock`, forcing process-level retries and 20 ms benchmark sleeps. A short in-process wait for commit should queue concurrent writers and improve throughput without changing long-lock failure behavior.
- Files changed: `cli/src/workdir_lock.rs`, `cli/src/commands/commit.rs`.
- Benchmark commands:
  - `cargo test -p oakvcs-cli test_diff_omits_large_text_hunks_but_keeps_path --test integration`
  - `cargo test -p oakvcs-cli test_switch_create --test integration`
  - `cargo test -p oakvcs-cli test_commit --test integration`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak --lanes core --runs 1 --noise-floor-runs 2`
  - `python3 scripts/parallel_contention.py --workers 8 --commits-per-worker 3 --modes shared_checkout --runs 5 --subjects oak_installed,oak_local --oak-installed-bin /Users/mrmrs/.local/bin/oak --oak-local-bin /Users/mrmrs/o/oak/target/release/oak --results /Users/mrmrs/o/oak-benchmarks/results/devloop/contention-confirm-20260610T094825`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak --lanes core,workflow,contention --runs 2 --noise-floor-runs 3`
- Result:
  - Targeted tests passed.
  - Five-run contention median improved from the earlier local-regressed sample (local 160.3/s vs installed 196.0/s) to local 296.9/s vs installed 153.3/s.
  - Final primary PASS at `results/devloop/20260610T095004Z/`.
  - Final primary `contention_shared_checkout_w8`: local 346.8/s vs installed 166.5/s, no lost updates, integrity pass.
- Verdict: kept.
- Next idea: none obvious with a stable primary PASS. Deeper confirmation had high workflow embedded-null noise and was treated as inconclusive.

## Final Scoreboard

- Final primary command: `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak --lanes core,workflow,contention --runs 2 --noise-floor-runs 3`
- Final primary verdict: PASS (`results/devloop/20260610T095004Z/`).
- No regressions vs Oak baseline.
- No new Git guardrail breaches.
- Best wins:
  - `large_asset_manifest/vcs.diff`: about 67 MB -> 1.7 KB output.
  - Core binary-heavy `diff.dirty`: about 16.8 MB -> less than 400 bytes output.
  - `switch -c` branch output: 129-143 bytes -> 49 bytes.
  - `wide_config_refactor/vcs.branch`: about 51 ms -> about 8-9 ms.
  - `contention_shared_checkout_w8`: final primary local 346.8 snapshots/s vs installed 166.5 snapshots/s.
- Tests:
  - `cargo test -p oakvcs-cli` passed.

## Inconclusive / Noisy Runs

- Deeper confirmation command: `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak --lanes core,workflow,contention --runs 3 --noise-floor-runs 5`
- `results/devloop/20260610T094158Z/` failed on one contention sample before the lock-wait fix.
- `results/devloop/20260610T094916Z/` failed workflow latency gates with a 32.0% embedded-null spread; identical-command noise was too high for a defensible latency decision. The subsequent primary run passed.

## Wild-Agent Run Baseline

- Benchmark repo: `/Users/mrmrs/o/oak-benchmarks`
- Oak repo: `/Users/mrmrs/o/oak-wild` (symlink to `/Users/mrmrs/o/oak`; requested path was absent)
- Branch: `wild-agent-optimization`
- Command: `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core,workflow,contention --runs 2 --noise-floor-runs 3 --results results/wild-agent/primary`
- Baseline result: `results/wild-agent/primary/20260610T095301Z/`, REGRESSED on `wide_config_refactor/setup.snapshot` latency (`61.0 -> 76.5 ms`) and `setup.total` latency (`70.2 -> 87.0 ms`), with no correctness or Git guardrail failures.
- Largest remaining output/token offenders:
  - `wide_config_refactor/vcs.status`: 22,954 bytes, 5,013 estimated tokens.
  - `wide_config_refactor/vcs.diff`: 141,480 bytes, 5,012 estimated tokens.
  - `wide_config_refactor/setup.snapshot` and `vcs.snapshot`: about 22.9 KB each, 5,016 estimated tokens each.
  - `tiny_text/diff.dirty`: 2,520 bytes, 642 estimated tokens.
- Bold hypotheses:
  - Plain non-TTY `oak diff` should default to stat output while interactive TTY keeps the browser and explicit `--print` keeps full hunks.
  - Plain non-TTY `oak status` should emit a bounded summary or porcelain rows instead of branch metadata plus every path.
  - Plain non-TTY `oak commit` should emit commit id and bounded counts instead of every changed path.
  - `oak log` should grow agent-compatible compact/default history modes and path/search filters to avoid repeated full-history dumps.
  - A combined agent snapshot command could collapse status+diff+commit into one correctness-preserving operation for future benchmark tracks.

## Experiment 6: Non-TTY `oak diff` Defaults To Stat

- Hypothesis: Agents usually need changed paths and rough line counts before committing, not full hunks. Plain `oak diff` in a non-TTY should use the existing stat renderer; interactive `oak diff` keeps the TUI and explicit `oak diff --print` keeps patch output.
- Expected mechanism: The benchmark invokes `oak diff` with stdout captured. Routing that path to `render_stat` preserves path coverage while replacing hunks with one row per file plus totals.
- Files changed: `cli/src/commands/diff.rs`, `cli/src/commands/mount/mod.rs`.
- Commands run:
  - `cargo test -p oakvcs-cli test_diff --test integration`
  - `cargo build -p oakvcs-cli --release`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core --runs 1 --noise-floor-runs 2 --results results/wild-agent/fast`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core,workflow,contention --runs 2 --noise-floor-runs 3 --results results/wild-agent/primary`
- Metrics before/after:
  - Fast filter PASS at `results/wild-agent/fast/20260610T095602Z/`.
  - Primary PASS at `results/wild-agent/primary/20260610T095611Z/`.
  - `tiny_text/diff.dirty`: 2,520 bytes / 642 tokens -> about 156 bytes / 50 tokens; -94.4% output bytes vs installed Oak and -92.7% tokens vs installed Oak.
  - `bugfix_test_loop/vcs.diff`: -89.5% output bytes and -86.4% estimated tokens vs installed Oak.
  - Binary-heavy core diffs remained at -100.0% output bytes vs installed Oak and improved token deltas to -99.3% to -99.5%.
- Verdict: kept.
- Next idea: compact plain non-TTY `oak status` and `oak commit` output, because `wide_config_refactor` still hits the admitted-token cap on each.

## Experiment 7: Compact Non-TTY `status` And `commit`

- Hypothesis: Agent-visible `oak status` and `oak commit` should not repeat branch metadata and hundreds of changed paths when stdout is captured. Clean status can match `git status --short` and print nothing; dirty status should show counts plus a bounded sample; commit should emit only the commit id plus counts.
- Expected mechanism: The benchmark captures stdout, so non-TTY branching cuts repeated path dumps while keeping interactive human output unchanged.
- Files changed: `cli/src/output.rs`, `cli/src/commands/status.rs`, `cli/src/commands/commit.rs`.
- Commands run:
  - `cargo test -p oakvcs-cli test_status --test integration`
  - `cargo test -p oakvcs-cli test_commit --test integration`
  - `cargo test -p oakvcs-cli output --lib`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core --runs 1 --noise-floor-runs 2 --results results/wild-agent/fast`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core,workflow,contention --runs 2 --noise-floor-runs 3 --results results/wild-agent/primary`
- Metrics before/after:
  - Fast filter PASS at `results/wild-agent/fast/20260610T095944Z/`.
  - Primary PASS at `results/wild-agent/primary/20260610T095952Z/`.
  - `wide_config_refactor/vcs.status`: 22,954 bytes / 5,013 tokens -> 737 bytes / 198 tokens.
  - `wide_config_refactor/setup.snapshot`: about 22,911 bytes / 5,016 tokens -> about 44 bytes / 27 tokens.
  - `wide_config_refactor/vcs.snapshot`: about 22,896 bytes / 5,016 tokens -> about 44 bytes / 27 tokens.
  - Core clean status rows: -100.0% output bytes and -81.4% tokens vs installed Oak.
  - Core snapshot rows: -66.7% to -83.1% output bytes vs installed Oak.
- Verdict: kept.
- Next idea: bound implicit non-TTY diff stat rows, because `wide_config_refactor/vcs.diff` still lists hundreds of files and remains at 25,881 bytes / 5,012 tokens.

## Experiment 8: Bound Implicit Non-TTY Diff Stat Rows

- Hypothesis: Switching plain non-TTY `oak diff` to stat output is not enough for wide refactors; a stat row per changed file still dumps hundreds of paths. The implicit agent fallback should keep totals and a bounded sample, while explicit `oak diff --stat` remains full.
- Expected mechanism: Preserve line-count totals over every changed file, but print only the first 20 stat rows plus a `... N more` trailer in the implicit non-TTY path.
- Files changed: `cli/src/commands/diff.rs`, `cli/src/commands/mount/mod.rs`.
- Commands run:
  - `cargo fmt -p oakvcs-cli`
  - `cargo test -p oakvcs-cli test_diff --test integration`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core --runs 1 --noise-floor-runs 2 --results results/wild-agent/fast`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core,workflow,contention --runs 2 --noise-floor-runs 3 --results results/wild-agent/primary`
- Metrics before/after:
  - Fast filter PASS at `results/wild-agent/fast/20260610T100203Z/`.
  - Primary PASS at `results/wild-agent/primary/20260610T100211Z/`.
  - `wide_config_refactor/vcs.diff`: 25,881 bytes / 5,012 tokens -> 882 bytes / 233 tokens.
  - `wide_config_refactor/workflow.total`: 28,929 bytes / 5,918 tokens -> 3,930 bytes / 1,139 tokens.
- Verdict: kept.
- Next idea: lower the bounded sample to the smallest useful number; 20 rows is still generous for an agent transcript.

## Experiment 9: Tighten Agent Samples To 5 Rows

- Hypothesis: Five changed-path samples plus exact totals should be enough for agent triage; the agent can ask for explicit full output when it needs all paths. This should cut the last wide-refactor status/diff overhead without affecting small repos.
- Expected mechanism: Change the non-TTY status sample and implicit non-TTY diff stat sample from 20 rows to 5 rows.
- Files changed: `cli/src/commands/status.rs`, `cli/src/commands/diff.rs`, `cli/src/commands/mount/mod.rs`.
- Commands run:
  - `cargo fmt -p oakvcs-cli`
  - `cargo test -p oakvcs-cli test_status --test integration`
  - `cargo test -p oakvcs-cli test_diff --test integration`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core --runs 1 --noise-floor-runs 2 --results results/wild-agent/fast`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core,workflow,contention --runs 2 --noise-floor-runs 3 --results results/wild-agent/primary`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core,workflow,contention --runs 3 --noise-floor-runs 5 --results results/wild-agent/confirm`
- Metrics before/after:
  - Fast filter PASS at `results/wild-agent/fast/20260610T100504Z/`.
  - Primary attempts `results/wild-agent/primary/20260610T100517Z/` and `results/wild-agent/primary/20260610T100606Z/` both REGRESSED on unrelated setup latency rows; no correctness, output, or tool-call failures.
  - Confirmation PASS at `results/wild-agent/confirm/20260610T100639Z/`.
  - `wide_config_refactor/vcs.status`: 737 bytes / 198 tokens -> 197 bytes / 63 tokens.
  - `wide_config_refactor/vcs.diff`: 882 bytes / 233 tokens -> 237 bytes / 72 tokens.
  - `wide_config_refactor/workflow.total`: 3,930 bytes / 1,139 tokens -> 2,745 bytes / 843 tokens.
- Verdict: kept; primary latency failures are logged as noisy/unrelated, and confirmation passed cleanly.
- Next idea: compact non-TTY `oak init` setup output, which is now one of the few remaining repeated VCS chatter sources.

## Experiment 10: Silent Non-TTY `oak init`

- Hypothesis: `oak init` setup prose is useful in a terminal but wasted in captured agent transcripts. For non-TTY stdout, a successful init can be silent like many scriptable Unix commands; failures still surface through stderr/exit code.
- Expected mechanism: Guard stdout-only init success/info messages behind stdout TTY detection while preserving interactive prompts when stdin is attached.
- Files changed: `cli/src/commands/init.rs`, `cli/tests/piped_output.rs`.
- Commands run:
  - `cargo fmt -p oakvcs-cli`
  - `cargo test -p oakvcs-cli init --test integration`
  - `cargo test -p oakvcs-cli piped --test piped_output`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core --runs 1 --noise-floor-runs 2 --results results/wild-agent/fast`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core,workflow,contention --runs 2 --noise-floor-runs 3 --results results/wild-agent/primary`
- Metrics before/after:
  - Fast filter PASS at `results/wild-agent/fast/20260610T101030Z/`.
  - Primary PASS at `results/wild-agent/primary/20260610T101039Z/`.
  - Core `repo.init`: -100.0% output bytes and -82.7% tokens vs installed Oak.
  - `wide_config_refactor/setup.repo_init`: 242 bytes / 73 tokens -> 0 bytes / 13 tokens.
  - `bugfix_test_loop/setup.repo_init`: 238 bytes / 72 tokens -> 0 bytes / 13 tokens.
- Verdict: kept.
- Next idea: compact piped `oak log -v`, the remaining VCS-heavy history row.

## Experiment 11: Compact Piped Verbose Log

- Hypothesis: `oak log -v` in a pipe should still include changed-file signal, but it does not need multi-line commit blocks with author/date labels. A one-line commit row plus a bounded file sample should preserve agent utility and reduce history archaeology output.
- Expected mechanism: Replace piped verbose block formatting with `format_commit_compact_verbose`; TTY log UI and JSON output are unchanged.
- Files changed: `cli/src/output.rs`, `cli/src/commands/log.rs`, `cli/tests/piped_output.rs`.
- Commands run:
  - `cargo fmt -p oakvcs-cli`
  - `cargo test -p oakvcs-cli log --test piped_output`
  - `cargo test -p oakvcs-cli test_log --test integration`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core --runs 1 --noise-floor-runs 2 --results results/wild-agent/fast`
  - `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core,workflow,contention --runs 2 --noise-floor-runs 3 --results results/wild-agent/primary`
- Metrics before/after:
  - Fast filter PASS at `results/wild-agent/fast/20260610T101248Z/`.
  - Primary PASS at `results/wild-agent/primary/20260610T101259Z/`.
  - `history_archaeology/history.file_history`: 703 bytes / 189 tokens -> 400 bytes / 113 tokens.
  - `history_archaeology/history.pickaxe`: 703 bytes / 189 tokens -> 400 bytes / 113 tokens.
  - `history_archaeology/history.show_last`: 241 bytes / 75 tokens -> 132 bytes / 47 tokens.
  - `history_archaeology/workflow.total`: 1,920 bytes / 535 tokens -> 1,205 bytes / 355 tokens.
- Verdict: kept.
- Next idea: final confirmation. Remaining largest measured rows are mostly non-VCS search/test/read/edit work; further VCS gains likely need new semantic commands (`oak agent summary`, path-filtered log/pickaxe, or workflow-fusing).

## Wild-Agent Final Confirmation

- Confirmation command: `python3 scripts/devloop.py --oak-repo /Users/mrmrs/o/oak-wild --lanes core,workflow,contention --runs 3 --noise-floor-runs 5 --results results/wild-agent/confirm`
- Result: PASS at `results/wild-agent/confirm/20260610T101357Z/`.
- Regressions vs Oak baseline: 0.
- New Git guardrail breaches: 0.
- Improvements reported by verdict: 137.
- Full test command: `cargo test -p oakvcs-cli`, passed after updating piped-output tests for the new compact `status` contract.

### Final Workflow Totals

Across the five workflow scenarios, summing the reported average rows:

| Subject | Avg ms total | Est tokens total | Output bytes total | Tool calls total |
| --- | ---: | ---: | ---: | ---: |
| Git | 923.6 | 5,924 | 21,685 | 41 |
| Oak installed | 716.3 | 23,204 | 67,356,131 | 37 |
| Oak local final | 608.8 | 2,409 | 7,233 | 37 |

Final Oak local vs run-start Oak local (`results/wild-agent/primary/20260610T095301Z/`):

- Tokens: 18,289 -> 2,409, 86.8% lower.
- Output bytes: 198,086 -> 7,233, 96.3% lower.
- Wall time: 586.8 ms -> 608.8 ms summed average workflow rows; this run is 3.7% slower than the start-run sample, below the benchmark's final PASS gates and still 34.1% faster than Git.
- Tool calls: unchanged at 37 total workflow calls.

Final Oak local vs installed Oak in confirmation:

- Tokens: 23,204 -> 2,409, 89.6% lower.
- Output bytes: 67,356,131 -> 7,233, 99.99% lower.
- Wall time: 716.3 ms -> 608.8 ms, 15.0% faster.
- Tool calls: unchanged at 37 total workflow calls.

Final Oak local vs Git in confirmation:

- Tokens: 5,924 -> 2,409, 59.3% lower.
- Output bytes: 21,685 -> 7,233, 66.6% lower.
- Wall time: 923.6 ms -> 608.8 ms, 34.1% faster.
- Tool calls: 41 -> 37, 9.8% fewer.

### Biggest Single Wins

- `large_asset_manifest/vcs.diff`: installed Oak 67,111,012 bytes / 5,008 tokens -> final Oak 167 bytes / 54 tokens; 99.9998% lower output and 98.9% lower tokens.
- `wide_config_refactor/vcs.diff`: run-start Oak 141,480 bytes / 5,012 tokens -> final Oak 237 bytes / 72 tokens; 99.8% lower output and 98.6% lower tokens.
- `wide_config_refactor/vcs.status`: run-start Oak 22,954 bytes / 5,013 tokens -> final Oak 197 bytes / 63 tokens; 99.1% lower output and 98.7% lower tokens.
- `wide_config_refactor/setup.snapshot` and `vcs.snapshot`: each about 22.9 KB / 5,016 tokens at run start -> 44 bytes / 27 tokens final; about 99.8% lower output and 99.5% lower tokens.
- Core average final: Oak local 25.6 tokens/op and 44.0 bytes/op vs installed Oak 516.1 tokens/op and 1,398,484.8 bytes/op; 95.0% lower tokens and effectively all pathological byte output removed.

### Experiment Reassessment

- Kept experiments 6-11. No implementation was reverted.
- Two primary attempts during experiment 9 failed on unrelated setup latency noise; confirmation passed cleanly with the tightened 5-row samples.
- Best product direction from the retained set: make captured stdout an explicit agent contract. TTY behavior stays human-friendly; non-TTY defaults should be compact, bounded, ANSI-free, and opt-in for full dumps.
- Most promising risky next idea: a semantic `oak agent summary` or fused workflow command that returns status, bounded diff stats, recent branch context, and commit/snapshot result in one JSON or one-line response. The current changes cut bytes/tokens hard, but workflow tool calls are unchanged; this is where the remaining ceiling likely sits.
- Remaining ceiling estimate: another 20-40% token reduction may be possible in workflow rows by collapsing repeated command text and status/diff/log calls; wall-clock ceiling is probably another 10-25% if fused commands avoid process startup and redundant tree scans. Further byte reduction is now mostly marginal except for explicit full-output modes and any remaining history/search dumps.

### Result Paths

- Baseline primary: `results/wild-agent/primary/20260610T095301Z/`
- Fast filters: `results/wild-agent/fast/20260610T095602Z/`, `results/wild-agent/fast/20260610T095944Z/`, `results/wild-agent/fast/20260610T100203Z/`, `results/wild-agent/fast/20260610T100504Z/`, `results/wild-agent/fast/20260610T101030Z/`, `results/wild-agent/fast/20260610T101248Z/`
- Primary winners: `results/wild-agent/primary/20260610T095611Z/`, `results/wild-agent/primary/20260610T095952Z/`, `results/wild-agent/primary/20260610T100211Z/`, `results/wild-agent/primary/20260610T101039Z/`, `results/wild-agent/primary/20260610T101259Z/`
- Noisy primary attempts: `results/wild-agent/primary/20260610T100517Z/`, `results/wild-agent/primary/20260610T100606Z/`
- Confirmation passes: `results/wild-agent/confirm/20260610T100639Z/`, `results/wild-agent/confirm/20260610T101357Z/`
