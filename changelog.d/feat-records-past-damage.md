### Added

- `records::inode_at` decodes an inode out of a walked inode table, so an
  inode is lost only when a block it spans is damaged or does not follow the
  block before it. It returns an `InodeRecord`: the inode, a directory's
  parent inode number, and the record's length.
- `records::listing_at` decodes a directory listing out of a walked directory
  table, keeping every entry before the first byte that cannot be read or
  makes no sense, with the fault and where it is.
- `dir::parse_listing_partial` keeps every entry before a fault in a listing,
  and names the fault's offset.

### Changed

- The strict inode and listing parsers are the same code as the
  damage-tolerant ones, refused on the first fault, with the same errors.
