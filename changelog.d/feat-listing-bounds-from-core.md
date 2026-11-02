### Changed
- `fs.squashfs ls --recursive` takes its arguments, help, finite defaults and bound-reached errors from rust-fs-core's shared listing interface; the defaults, traversal and errors remain unchanged.
- `fs.squashfs export --recursive` exits 5 instead of 1 when it left some items of a tree out but published others, through rust-fs-core 0.4.3; it still exits 1 when nothing was published.
