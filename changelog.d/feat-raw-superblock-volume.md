### Added

- `Volume` opens an image on its superblock alone, so the superblock, the
  device size, the compressor, the health of the image and its raw bytes are
  available however damaged the rest is. `Volume::filesystem` is the strict
  open of the same device.
- `Superblock::decode` reads the 96 bytes without judging any field, and
  `Superblock::faults` lists every field the strict open would refuse, each
  with its byte offset and width, as a `SuperblockFault` that converts to the
  strict error or to a `Damage` report.
- `Damage`, `DamageKind` and `Severity` describe damage the way the family's
  salvage convention does.

### Changed

- `Superblock::parse` and `Superblock::validate_against_device` refuse on the
  first of those faults, with the same errors as before.
