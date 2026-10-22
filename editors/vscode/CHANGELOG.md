# Changelog

## 0.1.0 (preview)

First release.

- Oak source control provider: Changes, Branch Changes and Merge Conflicts
  groups; gutter quick diff against HEAD; Explorer/tab change badges.
- The input box is the branch description; checkpoint all or selected files.
- Push button in the Source Control title bar, a status-bar indicator with the
  unpushed commit count, and a Push / Always Push / Don't Ask Again prompt
  after each checkpoint (`oak.promptToPushAfterCommit`); inline Checkpoint &
  Push on the Changes group.
- Discard, pull, fetch, sync, publish to ORG/REPO, server-side merge (with
  wait-for-CI), and finish.
- Branch switch / create / rename / close.
- Conflict resolution: accept ours / theirs, continue / abort.
- Oak History view with per-file history, commit diffs and restore.
- CI status, runs and logs; oak.space links; clone and init.
