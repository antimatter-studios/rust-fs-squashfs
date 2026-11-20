### Added

- `data::file_map` lays out a regular file's blocks from its inode alone:
  each full block stored, sparse, or in the tail fragment, with its extent.
- `data::read_file_block` reads one block and says what it holds: its bytes
  (and whether a compressed stream vouched for them), zeros the format
  records for a sparse block, or why its bytes are unknown. A block that
  cannot be read or decoded is missing, never zeros.
- `Volume::file_blocks` and `Volume::read_file_block` do that on an image.

### Changed

- The strict data read decodes through `data::decode_stored_block`, with the
  same errors as before.
