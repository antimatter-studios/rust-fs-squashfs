## Interfaces

| `fs.squashfs` `rm`, `rmdir`, `mv`, `ln` (including `-s`), `truncate`, `mkdir --parents`, `write --at` | Refused: "SquashFS is read-only" (exit 3), before opening the image or reading stdin | Unreleased (#179) | #179 | `tests/cli/test-fs.sh`, `cli_docs.rs` |
