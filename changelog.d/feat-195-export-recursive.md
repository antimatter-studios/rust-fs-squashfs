### Added

- `fs.squashfs TARGET export --recursive PATH DEST` copies a directory tree
  through rust-fs-core's shared tree transfer. Every item is reported as
  completed, skipped, refused or failed, and a partial tree exits 1.
  Existing directories are merged into, `--max-depth` bounds the walk, and
  special files are skipped. A file whose data cannot be read fails alone
  and is never published. `--preserve=links` recreates symlinks without
  following them and keeps hard links; `--preserve=times` sets the stored
  modification time, in whole seconds. `--verify` reads each staged file
  back before publication, for single files too. All of it is checked
  against the Linux kernel's mount and unsquashfs's extraction of trees 33
  directories deep (#195).

### Changed

- rust-fs-core is pinned at 0.3.15, the release carrying the shared tree
  transfer (#195).
