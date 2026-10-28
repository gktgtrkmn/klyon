# klyon

> **Experimental.** This is a research playground for file-integrity monitoring, not a
> production tool. Interfaces, storage format, and guarantees change without notice.

`klyon` is a file integrity monitor (FIM): it records a cryptographic baseline of a
directory tree and later tells you what was **added, modified, or deleted**. Files are
hashed with [BLAKE3](https://github.com/BLAKE3-team/BLAKE3) and scanned in parallel;
baselines and check history live in a single SQLite database.

## Build

```sh
cargo build --release
# binary at ./target/release/klyon
```

Or run through cargo: `cargo run -p klyon --release -- <command>`.

## Commands

A **baseline** is a named snapshot of one or more root paths.

```sh
# Create a baseline named "home" watching a directory (scans + records it now)
klyon create home /home/me

# List all baselines (file counts, when created, last checked)
klyon ls

# Check a baseline's roots against its stored snapshot
klyon check home

# Remove a baseline and its history
klyon remove home
```

Baselines can also be addressed by their uuid with `-u`:

```sh
klyon check  -u 1181f4a5-0a90-4153-84f9-17448ef68d26
klyon remove -u 1181f4a5-0a90-4153-84f9-17448ef68d26
```

`check` is read-only: it never overwrites the baseline, so a change stays flagged until
you explicitly re-`create` it.

## Storage

| What | Where |
|------|-------|
| Database | `~/.klyon/klyon.db` (override with `$KLYON_DB`) |
| Logs | `./.klyon/klyon.log.*` |

The database directory is excluded from scans, so watching a parent of `~/.klyon` won't
hash klyon's own files.

## Layout

- `core/` — shared types and the snapshot-diff logic
- `klyon/` — the CLI, scanner, and SQLite storage layer
