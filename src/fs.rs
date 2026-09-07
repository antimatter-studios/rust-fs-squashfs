//! Top-level read-only SquashFS filesystem handle.
//!
//! Ties the superblock, metadata reader, inode/dir parsers, and lookup
//! tables together into a path-addressable read API:
//! [`Filesystem::open`] → [`Filesystem::lookup_path`] →
//! [`Filesystem::read_file`] / [`Filesystem::read_dir`] /
//! [`Filesystem::read_symlink_target`].

use std::sync::Arc;

use crate::decompress::{self, Compressor};
use crate::dir::{self, DirEntry};
use crate::error::{Error, Result};
use crate::inode::Inode;
use crate::metablock::{MetaCache, MetaCursor, MetadataRef};
use crate::superblock::{self, Superblock};
use crate::table::{self, ExportTable, FragmentEntry};
use crate::xattr::{self, XattrEntry, XattrIdTable};
use fs_core::BlockRead;

/// How many blocks a mount caches by default.
///
/// SIZED IN THE ARCHIVE'S OWN BLOCK, WHICH IS LARGE. `mksquashfs`
/// defaults to 128 KiB, and images use up to 1 MiB, so the count here
/// buys far more memory per unit than the same number would in a
/// driver whose blocks are 4 KiB. 32 is 4 MiB at the usual size and
/// 32 MiB at the largest — already generous for metadata, which is
/// what this holds; file data passes through on the bypass rule.
///
/// The measurement in `docs/read-path-cost.md` was taken with this
/// value. Change it and re-take the measurement rather than the
/// reverse.
pub const DEFAULT_CACHE_BLOCKS: usize = 32;

/// How many **decompressed** metadata blocks a mount holds.
///
/// A different cache from the one above, sized in a different unit and
/// holding a different thing. That one holds the archive's blocks as
/// they sit on disk — still compressed — and 32 of them is megabytes.
/// This one holds metadata blocks *after* the codec has run, and a
/// metadata block decompresses to at most 8 KiB, so 256 of them is
/// 2 MiB.
///
/// 2 MiB is a lot of metadata: an image with tens of thousands of files
/// has a metadata table measured in low megabytes in total, and the
/// blocks a walk returns to are far fewer than that. The measurement in
/// `docs/read-path-cost.md` was taken at this value.
pub const DEFAULT_META_CACHE_BLOCKS: usize = 256;

pub struct Filesystem {
    dev: Arc<dyn BlockRead>,
    pub sb: Superblock,
    comp: Compressor,
    /// uid/gid id table — inode `uid_idx` / `gid_idx` index into this.
    id_table: Vec<u32>,
    /// Fragment table — a file's `fragment_index` indexes into this.
    fragments: Vec<FragmentEntry>,
    /// Extended-attribute id table, or `None` when the image was built
    /// with `-no-xattrs`. Read once at mount: it holds one entry per
    /// distinct SET of attributes in the image, not one per file, so it
    /// is small even when every file carries something.
    xattr_ids: Option<XattrIdTable>,
    /// Export table, or `None` for an image built with `-no-exports`.
    /// Only the pointer array is held; see [`ExportTable`].
    exports: Option<ExportTable>,
    /// Decompressed metadata blocks, keyed by their offset in the image.
    ///
    /// The block cache under `dev` stops the device being asked twice
    /// for the same bytes; this stops the codec being run twice over
    /// them. Both are needed, and this is the one that moves the clock:
    /// see `docs/read-path-cost.md`.
    meta_cache: MetaCache,
}

impl Filesystem {
    /// Open a SquashFS image over a block device. Reads + validates the
    /// superblock, rejects unsupported compressors up front (every
    /// metadata read needs the codec), and loads the small id + fragment
    /// tables once.
    pub fn open(dev: Arc<dyn BlockRead>) -> Result<Self> {
        Self::open_with_cache(dev, DEFAULT_CACHE_BLOCKS)
    }

    /// Open an image, caching `blocks` metadata blocks.
    ///
    /// # Why the cache is built here and not by the caller
    ///
    /// It is sized in blocks, and the block size is the image's. A
    /// caller wanting to wrap the device itself would have to parse a
    /// superblock first to know what to wrap it with — which is what
    /// this does, once, before wrapping.
    ///
    /// # What it is for
    ///
    /// Metadata lives in 8 KiB blocks, each compressed and each holding
    /// many inodes or directory entries. Resolving `/a/b/c` decompresses
    /// the block holding the root's entries, then `a`'s, then `b`'s —
    /// and the next path resolved does all of it again, from the device,
    /// for bytes that cannot change in a read-only archive.
    ///
    /// `blocks` of zero disables it, which is what the measurement in
    /// `tests/read_path_cost.rs` uses to take its baseline.
    ///
    /// # This is not the only cache
    ///
    /// `blocks` sizes the cache of the archive's blocks *as they sit on
    /// disk*, and it is the one that removes device reads. The mount
    /// also holds [`DEFAULT_META_CACHE_BLOCKS`] metadata blocks after
    /// decompression, and that is the one that removes codec work; see
    /// [`Filesystem::set_meta_cache_capacity`] to size or disable it.
    /// Zero here does not disable that one.
    pub fn open_with_cache(dev: Arc<dyn BlockRead>, blocks: usize) -> Result<Self> {
        let dev: Arc<dyn BlockRead> = if blocks == 0 {
            dev
        } else {
            // The block size is not known until the superblock has been
            // read, and the superblock is at offset zero, so this one
            // read goes to the device directly.
            let sb = superblock::read(&*dev)?;
            fs_core::CachingDevice::read_only(dev, u64::from(sb.block_size), blocks)
        };
        let sb = superblock::read(&*dev)?;
        // `compressor()` has already rejected an id this build does not
        // know, so the guard below cannot fire today: `is_supported` is
        // `true` for every codec, including legacy lzma, which is
        // best-effort but still attempted.
        //
        // Kept as the seam for a codec that is recognised but cannot be
        // decoded by this build — a feature-gated one, say. Deleting it
        // would mean rediscovering where that check belongs.
        let comp = sb.compressor()?;
        if !comp.is_supported() {
            return Err(Error::UnsupportedCompression(sb.compression_id));
        }
        let id_table = table::read_id_table(&*dev, &sb)?;
        let fragments = table::read_fragment_table(&*dev, &sb)?;
        let xattr_ids = xattr::read_id_table(&*dev, &sb)?;
        let exports = table::read_export_table(&*dev, &sb)?;
        Ok(Filesystem {
            dev,
            sb,
            comp,
            id_table,
            fragments,
            xattr_ids,
            exports,
            meta_cache: MetaCache::new(DEFAULT_META_CACHE_BLOCKS),
        })
    }

    /// Resize the decompressed-metadata cache, as a builder. Zero
    /// switches it off.
    pub fn with_meta_cache_capacity(self, blocks: usize) -> Self {
        self.set_meta_cache_capacity(blocks);
        self
    }

    /// Resize the decompressed-metadata cache in place. Zero switches it
    /// off; anything already held is dropped either way.
    pub fn set_meta_cache_capacity(&self, blocks: usize) {
        self.meta_cache.set_capacity(blocks);
    }

    /// `(entries, capacity, hits, misses)` for the decompressed-metadata
    /// cache. A miss is one metadata block put through the codec.
    pub fn meta_cache_stats(&self) -> (usize, usize, u64, u64) {
        self.meta_cache.stats()
    }

    /// The archive-wide compressor.
    pub fn compressor(&self) -> Compressor {
        self.comp
    }

    /// Resolve a uid index to its real uid (0 if out of range).
    pub fn resolve_uid(&self, idx: u16) -> u32 {
        self.id_table.get(idx as usize).copied().unwrap_or(0)
    }
    /// Resolve a gid index to its real gid (0 if out of range).
    pub fn resolve_gid(&self, idx: u16) -> u32 {
        self.id_table.get(idx as usize).copied().unwrap_or(0)
    }

    pub fn read_inode(&self, inode_ref: u64) -> Result<Inode> {
        Inode::read(&*self.dev, &self.sb, inode_ref, Some(&self.meta_cache))
    }

    /// Whether this image can resolve an inode number back to an inode.
    ///
    /// False for an image built with `mksquashfs -no-exports`, which
    /// carries no such map. A caller that hands out inode numbers and
    /// expects to be asked about them later should check this once at
    /// mount rather than discovering it on the first question.
    pub fn is_exportable(&self) -> bool {
        self.exports.is_some()
    }

    /// Resolve an inode number to its inode, through the export table.
    ///
    /// This is the only way in that does not start at the root. A caller
    /// holding a number and nothing else — an NFS file handle, or any
    /// identifier a layer above handed out earlier — would otherwise
    /// have to keep its own map of every inode it ever mentioned, or
    /// walk the tree again to find one.
    ///
    /// Inode numbers are 1-based and run to
    /// [`Superblock::inode_count`](crate::Superblock).
    ///
    /// # Errors
    ///
    /// [`Error::NotExportable`] when the image has no export table, and
    /// [`Error::NotFound`] for a number outside the image's range. The
    /// two are distinct because a caller can act on the difference: the
    /// first will never succeed for this image, the second might for a
    /// different number.
    pub fn read_inode_by_number(&self, inode_number: u32) -> Result<Inode> {
        self.read_inode(self.inode_ref_for_number(inode_number)?)
    }

    /// The packed metadata reference for an inode number, without
    /// reading the inode.
    ///
    /// # Errors
    ///
    /// As [`read_inode_by_number`](Self::read_inode_by_number).
    pub fn inode_ref_for_number(&self, inode_number: u32) -> Result<u64> {
        let exports = self.exports.as_ref().ok_or(Error::NotExportable)?;
        exports.lookup(&*self.dev, &self.sb, inode_number, Some(&self.meta_cache))
    }

    pub fn root_inode(&self) -> Result<Inode> {
        self.read_inode(self.sb.root_inode_ref)
    }

    /// List a directory's entries (linear order; `.`/`..` are implicit and
    /// not returned).
    pub fn read_dir(&self, inode: &Inode) -> Result<Vec<DirEntry>> {
        if !inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        // NOT BOUNDED AGAINST THE IMAGE, deliberately. A listing lives
        // in compressed metadata blocks, so its decompressed length
        // routinely exceeds the whole image: `mksquashfs` on a
        // directory of two thousand files produced a 41049-byte listing
        // inside a 20480-byte image. What bounds the memory here is
        // that `MetaCursor` grows its buffer one 8 KiB metablock at a
        // time and stops when a device read runs off the end, so the
        // cost is bounded by the metadata the image really holds.
        let listing_len = inode.dir_listing_len();
        if listing_len == 0 {
            return Ok(Vec::new());
        }
        // THE one copy of "a table start plus a block offset".
        //
        // This wrote the sum out by hand, which is how it came to be a
        // plain `+` while `MetadataRef::start_abs` — the same sum, over
        // the same two kinds of number — was saturating. Going through
        // the type means the rule cannot be right in one place and
        // wrong in another, and it is the reason `MetadataRef` exists:
        // "not hard to get right once; easy to get right differently
        // three times", as its own doc puts it.
        let start_abs = MetadataRef {
            block_offset: u64::from(inode.dir_start_block),
            in_block: inode.dir_block_offset,
        }
        .start_abs(self.sb.directory_table_start);
        let mut cur = MetaCursor::new(
            &*self.dev,
            &self.sb,
            start_abs,
            inode.dir_block_offset,
            Some(&self.meta_cache),
        )?;
        let buf = cur.read_exact(listing_len)?;
        dir::parse_listing(&buf)
    }

    /// Look up a single name in a directory (linear scan).
    pub fn lookup(&self, dir: &Inode, name: &[u8]) -> Result<Inode> {
        for e in self.read_dir(dir)? {
            if e.name == name {
                return self.read_inode(e.inode_ref);
            }
        }
        Err(Error::NotFound)
    }

    /// Resolve a `/`-separated path from the root. Symlinks are returned
    /// as-is (not followed) — FSKit/the kernel handles symlink expansion
    /// via `readlink`.
    pub fn lookup_path(&self, path: &str) -> Result<Inode> {
        let mut node = self.root_inode()?;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            if !node.is_dir() {
                return Err(Error::NotADirectory);
            }
            node = self.lookup(&node, comp.as_bytes())?;
        }
        Ok(node)
    }

    /// Every extended attribute on an inode, in the order the image
    /// stores them.
    ///
    /// Names come back assembled — `user.colour`, not `colour` — since
    /// SquashFS stores the namespace prefix as a small integer and the
    /// rest of the name after it, and a caller should not have to know
    /// that.
    ///
    /// An inode with no attributes gives an empty list, and so does
    /// every inode in an image built with `-no-xattrs`. Neither is an
    /// error: absence is a normal answer, and a caller cannot act on the
    /// difference between "none" and "could not look".
    ///
    /// # Errors
    ///
    /// As the metadata reader, plus [`Error::BadMetadata`] when the
    /// inode names an attribute set the image does not have or the set
    /// does not decode.
    pub fn list_xattrs(&self, inode: &Inode) -> Result<Vec<XattrEntry>> {
        let Some(table) = self.xattr_ids.as_ref() else {
            return Ok(Vec::new());
        };
        if !inode.has_xattrs() {
            return Ok(Vec::new());
        }
        xattr::read_set(
            &*self.dev,
            &self.sb,
            table,
            inode.xattr_index,
            Some(&self.meta_cache),
        )
    }

    /// One attribute's value, by assembled name (`user.colour`).
    ///
    /// `Ok(None)` means the attribute is not set, which is distinct from
    /// `Ok(Some(vec![]))` — a zero-length value is a real thing to store.
    ///
    /// # Why this reads the whole set
    ///
    /// Unlike the sister drivers there is no index to search: a set is a
    /// flat run of records and a name's position in it is not derivable
    /// from the name. The set is small — it is the attributes of one
    /// file — and the metadata block it lives in is almost certainly
    /// already decompressed in the cache, since the inode that named it
    /// was just read.
    pub fn get_xattr(&self, inode: &Inode, name: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .list_xattrs(inode)?
            .into_iter()
            .find(|e| e.name == name)
            .map(|e| e.value))
    }

    /// A symlink's raw target bytes.
    pub fn read_symlink_target(&self, inode: &Inode) -> Result<Vec<u8>> {
        if !inode.is_symlink() {
            return Err(Error::BadInode("read_symlink_target on non-symlink"));
        }
        Ok(inode.symlink_target.clone())
    }

    /// Read up to `buf.len()` bytes from a regular file starting at
    /// `offset`. Returns the number of bytes copied (0 at/after EOF;
    /// short when the read crosses EOF). EOF is the *only* reason for a
    /// short return: a data block that decodes to less than its logical
    /// length is corruption, and comes back as an error rather than as
    /// fewer bytes.
    pub fn read_file(&self, inode: &Inode, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if !inode.is_regular_file() {
            return Err(Error::BadInode("read_file on non-file"));
        }
        let size = inode.file_size;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let to_read = buf.len().min((size - offset) as usize);
        let bs = self.sb.block_size as u64;

        // Precompute each full block's absolute on-disk start once
        // (prefix sum of on-disk sizes) so per-block reads stay O(1).
        let block_offsets = self.block_offsets(inode);

        let mut written = 0usize;
        while written < to_read {
            let abs_pos = offset + written as u64;
            let block_idx = (abs_pos / bs) as usize;
            let block_off = (abs_pos % bs) as usize;

            let block = self.read_logical_block(inode, block_idx, &block_offsets)?;
            if block_off >= block.len() {
                // Nothing to copy and no progress to make. `read_logical_block`
                // returns a block of exactly the length this offset was
                // computed against, so reaching here means the image lied
                // about a block; the loop must not spin, and it must not
                // report the bytes copied so far as a complete read either.
                return Err(Error::BadInode(
                    "data block shorter than the offset read from it",
                ));
            }
            let take = (block.len() - block_off).min(to_read - written);
            buf[written..written + take].copy_from_slice(&block[block_off..block_off + take]);
            written += take;
        }
        Ok(written)
    }

    /// Absolute on-disk start offset of each full data block.
    fn block_offsets(&self, inode: &Inode) -> Vec<u64> {
        let mut offs = Vec::with_capacity(inode.block_sizes.len());
        // Saturating: `blocks_start` is a raw `u64` for an extended
        // file inode, so the running sum can leave a `u64` -- and in
        // release, where this crate ships with `overflow-checks` off,
        // it wrapped to a small offset that then reads some unrelated
        // part of the image as the file's data. Saturating gives an
        // offset no device reaches, so the read fails and says so.
        let mut cursor = inode.blocks_start;
        for &sz in &inode.block_sizes {
            offs.push(cursor);
            cursor = cursor.saturating_add(u64::from(table::data_on_disk_size(sz)));
        }
        offs
    }

    /// Decompress one logical block of a file: a full data block for
    /// `block_idx < block_sizes.len()`, otherwise the tail fragment.
    fn read_logical_block(
        &self,
        inode: &Inode,
        block_idx: usize,
        block_offsets: &[u64],
    ) -> Result<Vec<u8>> {
        let bs = self.sb.block_size as u64;
        let n_full = inode.block_sizes.len();

        if block_idx < n_full {
            let logical_len = bs.min(inode.file_size - block_idx as u64 * bs) as usize;
            let size_word = inode.block_sizes[block_idx];
            if table::data_on_disk_size(size_word) == 0 {
                // Sparse block — all zeros, no on-disk payload.
                return Ok(vec![0u8; logical_len]);
            }
            let block = self.read_data_block(block_offsets[block_idx], size_word, logical_len)?;
            // A full data block's decoded length is not a matter of opinion:
            // the file size and the block geometry fix it exactly. A codec
            // cannot check this for us — a stream that ends cleanly after
            // fewer bytes than it should is well-formed as far as the
            // decoder is concerned — and this is the only place that knows
            // the expected length, so it is the only place that can refuse.
            if block.len() != logical_len {
                return Err(Error::BadInode("data block decoded to the wrong length"));
            }
            Ok(block)
        } else if inode.has_fragment() {
            let frag = self
                .fragments
                .get(inode.fragment_index as usize)
                .ok_or(Error::BadInode("fragment index out of range"))?;
            let frag_block =
                self.read_data_block(frag.start, frag.size, self.sb.block_size as usize)?;
            let tail_len = (inode.file_size - n_full as u64 * bs) as usize;
            let start = inode.fragment_offset as usize;
            let end = start
                .checked_add(tail_len)
                .ok_or(Error::BadInode("fragment offset overflow"))?;
            if end > frag_block.len() {
                return Err(Error::BadInode("fragment tail past fragment block end"));
            }
            Ok(frag_block[start..end].to_vec())
        } else {
            Err(Error::OutOfRange)
        }
    }

    /// Read + decompress one data block. `size_word` carries the on-disk
    /// size + compressed bit; `max_out` bounds the decompressed length.
    fn read_data_block(&self, abs_off: u64, size_word: u32, max_out: usize) -> Result<Vec<u8>> {
        let on_disk = table::data_on_disk_size(size_word) as usize;
        // THE SIZE WORD IS THE ALLOCATION, and it comes off the disk.
        //
        // `DATA_SIZE_MASK` is 24 bits, so `on_disk` reaches 16 MiB
        // while `block_size` is at most 1 MiB. A block whose compressed
        // form is larger than the block it decompresses to is corrupt
        // by definition — `mksquashfs` stores such a block uncompressed
        // instead — so the ceiling is the archive's own block size.
        //
        // The metadata sibling has had this bound all along
        // (`metablock.rs`: `on_disk == 0 || on_disk > METADATA_SIZE`).
        // Without it here the read still failed, on the short read or
        // the decoder's own ceiling, but only after allocating and
        // reading up to 16 MiB per logical block, inside a read loop.
        if on_disk > self.sb.block_size as usize {
            return Err(Error::BadInode(
                "data block's on-disk size exceeds the archive's block size",
            ));
        }
        let mut raw = vec![0u8; on_disk];
        self.dev.read_at(abs_off, &mut raw)?;
        if table::data_is_compressed(size_word) {
            decompress::decompress(self.comp, &raw, max_out)
        } else {
            if raw.len() > max_out {
                return Err(Error::BadInode(
                    "uncompressed data block exceeds block size",
                ));
            }
            Ok(raw)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inode::TYPE_BASIC_FILE;
    use crate::metablock::tests::MemDev;
    use crate::superblock::tests::synth_sb;
    use flate2::{Compress, Compression, FlushCompress};
    use std::sync::Mutex;

    const BLOCK_LOG: u16 = 12;
    const BLOCK_SIZE: usize = 1 << BLOCK_LOG;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut enc = Compress::new(Compression::default(), true);
        let mut out = vec![0u8; data.len() + 64];
        enc.compress(data, &mut out, FlushCompress::Finish).unwrap();
        out.truncate(enc.total_out() as usize);
        out
    }

    /// A `Filesystem` over raw bytes. The file-read path touches only the
    /// superblock and the device, so the lookup tables stay empty.
    fn fs_over(bytes: Vec<u8>) -> Filesystem {
        Filesystem {
            dev: Arc::new(MemDev(Mutex::new(bytes))),
            sb: Superblock::parse(&synth_sb(BLOCK_LOG, 0, 0)).unwrap(),
            comp: Compressor::Gzip,
            id_table: Vec::new(),
            fragments: Vec::new(),
            xattr_ids: None,
            exports: None,
            meta_cache: MetaCache::new(DEFAULT_META_CACHE_BLOCKS),
        }
    }

    /// A `Filesystem` whose superblock names `directory_table_start`.
    ///
    /// `synth_sb` does not take that field, and the directory sum is the
    /// one this test needs to reach.
    fn fs_over_with_dir_table(bytes: Vec<u8>, directory_table_start: u64) -> Filesystem {
        let mut sb_bytes = synth_sb(BLOCK_LOG, 0, 0);
        sb_bytes[0x48..0x50].copy_from_slice(&directory_table_start.to_le_bytes());
        Filesystem {
            dev: Arc::new(MemDev(Mutex::new(bytes))),
            sb: Superblock::parse(&sb_bytes).unwrap(),
            comp: Compressor::Gzip,
            id_table: Vec::new(),
            fragments: Vec::new(),
            xattr_ids: None,
            exports: None,
            meta_cache: MetaCache::new(DEFAULT_META_CACHE_BLOCKS),
        }
    }

    /// The directory listing's start is `directory_table_start` plus the
    /// inode's `dir_start_block`, and that sum must saturate.
    ///
    /// The committed image cannot reach this: its root inode has
    /// `dir_start_block == 0`, so the sum is `a + 0`, which is identical
    /// under a plain `+` and a `saturating_add` for every `a`. An
    /// integration test over that fixture is a real test of the offset
    /// the read names and no test at all of the sum. Both addends have
    /// to be non-zero and large, and the inode's field is reachable only
    /// from here.
    ///
    /// The assertion is the offset, not the error variant: saturating
    /// puts the read at `u64::MAX`, which no device reaches, while
    /// wrapping puts it at a small number that names real bytes.
    #[test]
    fn the_directory_listing_start_saturates_rather_than_wrapping() {
        let fs = fs_over_with_dir_table(vec![0u8; 64 * 1024], u64::MAX - 1000);

        let mut dir = file_inode(100, Vec::new());
        dir.inode_type = crate::inode::TYPE_BASIC_DIR;
        dir.dir_start_block = u32::MAX;
        dir.dir_block_offset = 0;

        match fs.read_dir(&dir) {
            Err(Error::Block(fs_core::Error::ShortRead { offset, .. })) => assert_eq!(
                offset,
                u64::MAX,
                "the listing must be sought at the saturated offset, not a wrapped one"
            ),
            other => panic!("expected a short read at u64::MAX, got {other:?}"),
        }
    }

    /// The same sum with an ordinary pair of addends still adds.
    ///
    /// Without this, replacing the sum with a constant `u64::MAX` would
    /// pass the test above.
    #[test]
    fn an_ordinary_directory_listing_start_is_the_sum_of_its_parts() {
        let fs = fs_over_with_dir_table(vec![0u8; 64 * 1024], 1000);

        let mut dir = file_inode(100, Vec::new());
        dir.inode_type = crate::inode::TYPE_BASIC_DIR;
        dir.dir_start_block = 24;
        dir.dir_block_offset = 0;

        // 1024 is inside the 64 KiB buffer, so the read reaches the
        // device and fails on the zeros there rather than on its offset.
        match fs.read_dir(&dir) {
            Err(Error::Block(fs_core::Error::ShortRead { offset, .. })) => {
                panic!("expected the read to reach offset 1024, but it short-read at {offset}")
            }
            Err(_) => {}
            Ok(_) => panic!("a listing of zeros should not parse"),
        }
    }

    /// A regular-file inode whose data blocks start at offset 0.
    fn file_inode(file_size: u64, block_sizes: Vec<u32>) -> Inode {
        Inode {
            inode_type: TYPE_BASIC_FILE,
            permissions: 0o644,
            uid_idx: 0,
            gid_idx: 0,
            mtime: 0,
            inode_number: 1,
            nlink: 1,
            file_size,
            dir_start_block: 0,
            dir_block_offset: 0,
            blocks_start: 0,
            fragment_index: crate::superblock::SQUASHFS_INVALID_FRAG,
            fragment_offset: 0,
            block_sizes,
            symlink_target: Vec::new(),
            xattr_index: crate::xattr::SQUASHFS_INVALID_XATTR,
        }
    }

    /// A directory's listing decompresses out of metadata blocks, so
    /// its length is not bounded by the image: `mksquashfs` on a
    /// directory of two thousand files produces a 41049-byte listing
    /// inside a 20480-byte image. A bound against `bytes_used` was
    /// tried and refused exactly that, which is why this test exists
    /// rather than that bound.
    ///
    /// What bounds the memory is `MetaCursor`, which grows its buffer
    /// one 8 KiB metablock at a time and stops when a device read runs
    /// off the end of the image.
    #[test]
    fn a_listing_longer_than_the_image_still_reads_and_still_terminates() {
        let fs = fs_over(vec![0u8; 64 * 1024]);

        let mut dir = file_inode(0, Vec::new());
        dir.inode_type = crate::inode::TYPE_BASIC_DIR;
        dir.file_size = u64::from(u32::MAX);

        // The refusal comes from the device running out, not from the
        // declared length -- and it comes back rather than hanging or
        // allocating four gigabytes.
        let why = format!("{:?}", fs.read_dir(&dir).err());
        assert!(
            !why.contains("longer than the filesystem"),
            "a listing longer than the image was refused on its declared length: {why}"
        );
        assert!(why != "None", "a 4 GiB listing in a 64 KiB image succeeded");
    }

    /// A data block whose declared on-disk size is bigger than the
    /// archive's own block size is refused before it is allocated.
    ///
    /// `DATA_SIZE_MASK` is 24 bits, so the word reaches 16 MiB while
    /// `block_size` is at most 1 MiB. A block whose compressed form is
    /// larger than what it decompresses to is corrupt by definition —
    /// `mksquashfs` stores such a block uncompressed instead. Without
    /// the bound the read still failed, on the short read or the
    /// decoder's ceiling, but only after allocating and reading up to
    /// 16 MiB per logical block, inside a read loop.
    ///
    /// The metadata sibling has had this bound all along; this is the
    /// data path catching up.
    #[test]
    fn a_data_block_bigger_than_the_archives_block_size_is_refused() {
        let fs = fs_over(vec![0u8; 64 * 1024]);
        // 0x00FF_FFFF is the largest the 24-bit field can say, and the
        // archive's blocks are 4 KiB.
        let inode = file_inode(BLOCK_SIZE as u64, vec![0x00FF_FFFF]);
        let mut buf = vec![0u8; BLOCK_SIZE];
        match fs.read_file(&inode, 0, &mut buf) {
            Err(Error::BadInode(m)) => assert!(
                m.contains("on-disk size"),
                "the refusal must name the size word, got {m:?}"
            ),
            other => panic!("expected a refusal naming the size, got {other:?}"),
        }
    }

    /// The last on-disk size that must still be accepted.
    ///
    /// A block that compresses to exactly the archive's block size is
    /// legal — it is what an incompressible block looks like — so the
    /// bound is `>` and not `>=`. Without this, tightening it by one
    /// byte would refuse the commonest block in an incompressible file
    /// and pass the test above.
    #[test]
    fn a_data_block_of_exactly_the_block_size_is_accepted() {
        let payload = vec![0xABu8; BLOCK_SIZE];
        let fs = fs_over(payload.clone());
        // Bit 24 set = stored uncompressed, so the bytes come back as
        // they are and the test does not depend on a codec.
        let size_word = BLOCK_SIZE as u32 | crate::table::DATA_UNCOMPRESSED_BIT;
        let inode = file_inode(BLOCK_SIZE as u64, vec![size_word]);
        let mut buf = vec![0u8; BLOCK_SIZE];
        let n = fs
            .read_file(&inode, 0, &mut buf)
            .expect("a full-size uncompressed block is legal");
        assert_eq!(n, BLOCK_SIZE);
        assert_eq!(buf, payload);
    }

    #[test]
    fn short_data_block_is_an_error_not_a_short_read() {
        // A *well-formed* zlib stream that decodes to far fewer bytes than
        // the block it stands in. No codec can catch this — the stream is
        // valid and ends cleanly — so the only place that can is the read
        // path, which knows how long the block is supposed to be.
        //
        // Reported as a successful 5-byte read before this was fixed:
        // indistinguishable, to a caller, from a legitimate read at EOF.
        let stream = zlib(b"short");
        let mut img = stream.clone();
        img.resize(BLOCK_SIZE, 0); // the rest of the on-disk block slot
        let fs = fs_over(img);
        // Size word: low 24 bits = on-disk length, bit 24 clear = compressed.
        let inode = file_inode(BLOCK_SIZE as u64, vec![stream.len() as u32]);

        let mut buf = vec![0u8; BLOCK_SIZE];
        match fs.read_file(&inode, 0, &mut buf) {
            Err(e) => assert!(matches!(e, Error::BadInode(_)), "{e:?}"),
            Ok(n) => panic!(
                "corrupt data block reported as a successful short read of \
                 {n} bytes (file is {BLOCK_SIZE} bytes)"
            ),
        }
    }

    #[test]
    fn whole_blocks_still_read_back_intact() {
        // The guard above must not reject a healthy file: one full block
        // plus a partial last block, which is the shape every non-fragment
        // file has.
        let payload: Vec<u8> = (0..BLOCK_SIZE + 100).map(|i| (i % 251) as u8).collect();
        let first = zlib(&payload[..BLOCK_SIZE]);
        let last = zlib(&payload[BLOCK_SIZE..]);
        let mut img = first.clone();
        img.extend_from_slice(&last);
        let fs = fs_over(img);
        let inode = file_inode(
            payload.len() as u64,
            vec![first.len() as u32, last.len() as u32],
        );

        let mut buf = vec![0u8; payload.len()];
        let n = fs.read_file(&inode, 0, &mut buf).unwrap();
        assert_eq!(n, payload.len());
        assert_eq!(buf, payload);
    }
}
