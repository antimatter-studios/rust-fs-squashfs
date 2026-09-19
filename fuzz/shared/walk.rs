// Shared by both tiers, included textually rather than depended on.
//
// `tests/fuzz_decoders.rs` and `fuzz/src/lib.rs` both `include!` this
// file. A crate dependency would have been tidier, but the fuzz crate
// depends on `libfuzzer-sys`, which builds libFuzzer's C++ runtime --
// and making the gate depend on the fuzz crate would drag that into
// every pull request build on the stable toolchain, for a helper that
// is sixty lines of read-only walking.
//
// What matters is that the two tiers walk an image identically. If they
// did not, a corpus entry would mean two different things depending on
// which tier read it, and a reproducer from one would not reproduce in
// the other.

// Fully qualified below rather than imported: this file is `include!`d
// into modules that already import `Arc` and friends, and a duplicate
// `use` is a hard error.
use fs_core::{BlockRead, Result as BlockResult};

/// An image held in memory, presented as a device.
pub struct Bytes(pub Vec<u8>);

impl BlockRead for Bytes {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> BlockResult<()> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let end = start.saturating_add(buf.len());
        if end > self.0.len() {
            // What a real device answers for a read that runs off the
            // end, so a crafted image cannot be told apart from a short
            // one by which error it provokes.
            return Err(fs_core::Error::ShortRead {
                offset,
                want: buf.len(),
                got: self.0.len().saturating_sub(start),
            });
        }
        buf.copy_from_slice(&self.0[start..end]);
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

/// How many directory entries one walk will visit.
///
/// A crafted image can claim a directory of any size, and following all
/// of it would make a case slow rather than failing it -- which reads
/// as a hang without being one.
pub const ENTRY_BUDGET: usize = 64;

/// How much of any one file a walk will read.
pub const READ_BUDGET: usize = 65_536;

/// Open an image and walk what a reader would touch.
///
/// Every result is discarded. A crafted image is *supposed* to be
/// refused; what it may not do is panic, hang, or read somebody else's
/// memory. This is also the only way the metablock cache, the id,
/// fragment and xattr tables and the export lookup get fuzzed at all --
/// none of them takes a byte slice, so none can be a target on its own.
pub fn walk(image: &[u8]) {
    let dev: std::sync::Arc<dyn BlockRead> = std::sync::Arc::new(Bytes(image.to_vec()));
    let Ok(fs) = fs_squashfs::Filesystem::open(dev) else {
        return;
    };

    let _ = fs.compressor();
    let _ = fs.is_exportable();
    for idx in 0u16..8 {
        let _ = fs.resolve_uid(idx);
        let _ = fs.resolve_gid(idx);
    }

    let Ok(root) = fs.root_inode() else {
        return;
    };
    let _ = fs.list_xattrs(&root);

    let Ok(entries) = fs.read_dir(&root) else {
        return;
    };

    for entry in entries.iter().take(ENTRY_BUDGET) {
        let Ok(inode) = fs.read_inode(entry.inode_ref) else {
            continue;
        };
        let _ = fs.list_xattrs(&inode);
        let _ = fs.get_xattr(&inode, b"user.colour");

        if inode.is_symlink() {
            let _ = fs.read_symlink_target(&inode);
            continue;
        }
        if inode.is_dir() {
            let _ = fs.read_dir(&inode);
            continue;
        }
        let mut buf = vec![0u8; READ_BUDGET];
        let _ = fs.read_file(&inode, 0, &mut buf);
    }
}
