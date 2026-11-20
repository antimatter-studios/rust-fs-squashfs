### Added

- `salvage::Salvage` holds the recovery verbs over a `Volume`, built once
  from the walked inode and directory tables and the lookup tables.
- `Salvage::find_candidates` finds every inode that decodes three ways and
  merges them: a scan of the decoded inode table, which needs no reference to
  an inode, the export table, and the directories reachable from the root.
  Each candidate says how it was found; references that name a lost inode,
  and inode table the scan passed over, are reported.
- `Salvage::rebuild_tree` records every parent claim, from each listing that
  names a candidate and each directory inode's own parent field, and leaves
  orphans and conflicts as they are.
- `Salvage::capabilities` reports deleted candidates, deleted extents and
  cluster steps as unsupported: SquashFS deletes nothing and has no
  allocation units.
- `records::scan_inodes` finds inodes in a walked inode table without
  references, finding the record boundary again after damage.
