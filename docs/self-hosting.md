# Self-hosting with `oak serve`

`oak serve` is a self-hostable Oak server built into the CLI binary. It speaks
the same push/pull/clone protocol as oak.space, stores each repository in one
SQLite file, and needs no database server, object store, or accounts.

```bash
oak serve -d /srv/oak-data                               # loopback only, no token
OAK_SERVE_TOKEN=$(openssl rand -hex 32) \
  oak serve -d /srv/oak-data --host 0.0.0.0 -p 8080      # network-reachable: token required
```

Clients point at it like any remote:

```bash
oak login -r http://oak.internal:8080                    # paste the token once in the browser page
oak push -r http://oak.internal:8080 --repo acme/app     # first publication creates the repo
oak clone http://oak.internal:8080/acme/app
```

## What works

| Area | Commands |
|---|---|
| Sync | `push` (legacy and staged-v1, chunked large files), `pull`, `clone` (`--shallow`, `--path`, `--branch --expected-head`), `mount` |
| Branches | `oak branch list --remote`, `branch rename`, `close --remote`, `oak desc` sync, `merge` (server-side squash with three-way text merge, 409 + `conflict_paths` on overlap) |
| History and review | `oak log --remote`, `oak diff --remote`, `oak branch train`, `oak file --remote` (raw reads) |
| Integrity | `oak doctor`, `oak blob info`, snapshot-pinned clone preflight |
| Repos | `oak repo list` (`sort=name\|updated`), `oak repo visibility`, create on first push |
| Repo admin API | `DELETE /api/{owner}/{name}`, `POST /api/{owner}/{name}/transfer` (`{"to_user"\|"to_organization": "<owner>"}`), `GET/POST /api/orgs`, `GET /api/orgs/{owner}` |
| Auth | `--token` shared bearer secret; `oak login` browser page; `oak auth status` |

## What doesn't (by design)

- **No users, orgs, or permissions.** The only access control is `--token`. An
  owner is just a directory, and the "organizations" `/api/orgs` lists are
  those directories. A repo's visibility is a stored label that `oak repo
  visibility` reports and changes, but it is not enforced: anyone holding the
  token can read and write every repo.
- **No CI.** `oak ci runs` is always empty, and merges are never CI-gated.
- **No web UI.** Review links that `push`/`finish` print point at pages
  `oak serve` doesn't have.
- **No findings, feedback, sites, path permissions, webhooks, or git
  smart-HTTP.** Use `oak export` to produce a git repository.

## Operations

- **Layout:** `<dir>/<owner>/<name>.oakdb`, plus SQLite `-wal`/`-shm`
  sidecars while the server runs.
- **Backups:** copy each `.oakdb` file with SQLite's online backup, e.g.
  `sqlite3 app.oakdb ".backup app.bak"`. Don't copy the raw file while the
  server is writing to it.
- **Delete and move repos through the API**, not with `rm`/`mv` while the
  server is running: the server pools open connections per repo file.
  `DELETE` and `transfer` refuse with 409 while another request is using
  the repo.
- **TLS:** put a reverse proxy (Caddy, nginx) in front of `oak serve` for
  HTTPS. The server itself speaks plain HTTP.
- **Compression:** JSON responses over 32 KiB are gzipped for clients that
  accept it, as the `oak` CLI does. Chunk bytes are sent as-is.

## Performance

`scripts/bench-serve/bench.py` measures a running `oak serve` with synthetic
repos (no corpora needed). Results and the remaining bottlenecks are in
[`scripts/bench-serve/REPORT.md`](../scripts/bench-serve/REPORT.md).
