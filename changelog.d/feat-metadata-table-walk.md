### Added

- `metatable::read_metablock` reads one metadata block on its own and says
  what it holds: its decoded bytes, or a bad header, no compressor, a payload
  that does not decode, or an unreadable range. It never fails on damage.
- `metatable::walk_table` frames a metadata table block by block, carrying on
  past a damaged block: a header in range is trusted only when the block after
  it is whole, and a lost framing is found again at the next block that
  decodes to a full 8 KiB. Every damaged block, every run of bytes that frames
  into no block, and every short block before the last is reported.
- `Volume::inode_table` and `Volume::directory_table` walk the image's two
  metadata tables that way.

### Changed

- The strict metadata read is built on `read_metablock`, with the same errors
  as before.
