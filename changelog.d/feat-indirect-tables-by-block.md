### Added

- `indirect` reads the id, fragment, export and xattr id tables one metadata
  block at a time. An entry is present when its block decoded and missing,
  with the reason reported, when it did not, so a damaged block loses only
  its own entries. Pointers outside the image, repeated pointers, an index
  without room for its count and blocks too short for their entries are all
  reported. `Volume::id_table`, `fragment_table`, `export_table` and
  `xattr_id_table` read the image's tables that way.
