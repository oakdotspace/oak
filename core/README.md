# oak-core

Core version-control primitives for [Oak](https://oak.space) — version
control at the speed of agents.

`oak-core` is the shared foundation used by both the Oak CLI and the Oak
server. It is deliberately free of any storage, network, or server
concerns so the same content model and hashing rules are used on both
sides of a `push`/`pull`.

## What's in here

- **Content hashing** — BLAKE3-based, content-addressed object IDs.
- **Content-defined chunking** — [`fastcdc`](https://crates.io/crates/fastcdc)-based
  splitting for efficient large-file storage and deduplication.
- **Data model** — `Blob`, `Manifest`, `Commit`, `Tree`, `Branch`, `Tag`,
  and the related wire types that travel between client and server.
- **Diff & merge** — line-level diffing and 3-way merge helpers.
- **Ignore handling** — `.gitignore`/`.ignore`-style pattern matching.

Because these types define both the on-disk hashes and the sync wire
format, the CLI and server must build against the *same* `oak-core` —
that is exactly why it lives in its own crate.

## Usage

Published on crates.io as **`oakvcs-core`** (the `oak-core` name was taken),
but the library is imported as `oak_core`:

```toml
[dependencies]
oakvcs-core = "0.98"
```

```rust
use oak_core::{Blob, Manifest, Commit};
```

Server-side consumers that only need the data model can skip the local
SQLite/git layer:

```toml
[dependencies]
oakvcs-core = { version = "0.98", default-features = false }
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
