### Added

- `Salvage::walk_extents` lists a file's stored extents in order, ending at a
  block stored past the end of the image or a tail whose fragment is not in
  the table.
- `Salvage::report_damage` audits the superblock, both metadata tables, the
  lookup tables, inode table the candidate scan passed over, references to
  lost inodes and listings cut short, and with `check_data` every data and
  fragment block.
- `Salvage::read_extent` reads source bytes inside an extent.
- `Salvage::materialize` writes a file or symlink from what survives and
  reports the source of every range. A missing block is never passed off as
  the file's: the output stops before it, or holds zeros the report names.
