### Added

- `layers::SquashfsVolume` implements rust-fs-core's shared three layers:
  `VolumeAPI` and `OpenVolume` (open strictly, or for salvage on a readable
  superblock alone), `PrimitiveAPI` (the structures at known locations, and
  the salvage adapter), and `FileAPI` over the strict reader.
  `layers::SquashfsAdapter` implements `SalvageAdapter` over the crate's own
  damage-tolerant primitives and salvage verbs, with compressed blocks as
  encoded extents. Deleted candidates, deleted extents, cluster steps and
  geometry hypotheses are reported unsupported. A fragment tail is a window
  of its decoded fragment block.

### Changed

- rust-fs-core is pinned at 0.5.1, with its `layers` feature.
