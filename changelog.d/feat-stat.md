### Added

- `fs.squashfs TARGET stat PATH` reports a path's inode, type, permissions,
  owner, link count, size and modification time, plus a symlink's target or
  a device's major and minor numbers. SquashFS layout metadata is nested
  under `squashfs`. Unstored times, nanoseconds, allocation totals and
  inapplicable layout fields are null. Paths retain their byte identity,
  symlinks are reported without following them, and `--text` prints key/value
  lines (#177).
