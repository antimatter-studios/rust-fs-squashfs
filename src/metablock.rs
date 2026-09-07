//! SquashFS metadata-block reader.
//!
//! Inodes, directory listings, and the indirect lookup tables are all
//! stored in **metadata blocks**: each is at most 8 KiB when decompressed
//! and is preceded on disk by a `u16` little-endian header —
//!
//! - bit 15 (`0x8000`) set  → the payload is stored UNCOMPRESSED
//! - bits 0..14             → the on-disk size of the (possibly
//!   compressed) payload that follows the 2-byte header
//!
//! A single inode or directory record can straddle a metadata-block
//! boundary, so [`MetaCursor`] presents a flat byte stream that pulls and
//! decompresses successive metadata blocks on demand.
//!
//! [`MetaCache`] holds the *decompressed* result of that work, so the
//! second read of a block skips the codec as well as the device.

use crate::decompress;
use crate::error::{Error, Result};
use crate::superblock::{Superblock, METADATA_SIZE};
use fs_core::BlockRead;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

/// Set means the block is stored UNCOMPRESSED.
///
/// The polarity is the format's, not a choice made here, and it is the
/// opposite of what the obvious name would suggest — which is why the
/// name says what the bit means rather than what it is about.
const UNCOMPRESSED_BIT: u16 = 0x8000;
const SIZE_MASK: u16 = 0x7FFF;

/// One decompressed metadata block, plus where the next one starts.
///
/// The successor offset is cached alongside the bytes because it is not
/// derivable from them: it depends on the block's *on-disk* length, which
/// is in the 2-byte header a cache hit never reads.
#[derive(Clone)]
struct CachedBlock {
    /// Shared so a hit hands out the bytes without copying them.
    bytes: Arc<Vec<u8>>,
    next_abs: u64,
}

/// A cache of **decompressed** metadata blocks, keyed by the absolute
/// offset of the block's 2-byte header.
///
/// # Why this exists above the codec and not below it
///
/// A block cache under the driver (`fs_core::CachingDevice`) removes the
/// device reads and nothing else: the same 8 KiB of gzip is handed to the
/// decompressor again on every access. Measured on the fixture in
/// `tests/read_path_cost.rs`, taking the device reads to *zero* moved the
/// wall clock by nine percent. The read path is not I/O-bound, it is
/// bound by decompression, and only a cache that holds the decompressed
/// bytes can touch that.
///
/// # Why there is no invalidation
///
/// SquashFS is a read-only archive. The bytes at a given offset cannot
/// change while the image is mounted, so an entry can never go stale and
/// the key can be the offset alone. That is a real simplification over
/// the equivalent cache in a writable driver, and it is a property of the
/// format rather than an assumption being made here.
pub struct MetaCache {
    inner: Mutex<CacheInner>,
}

struct CacheInner {
    /// `None` when the capacity is zero, i.e. caching is switched off.
    /// Kept inside the same mutex as the counters so the disabled state
    /// costs a lock and nothing more.
    lru: Option<LruCache<u64, CachedBlock>>,
    hits: u64,
    misses: u64,
}

impl MetaCache {
    /// A cache holding at most `capacity` decompressed blocks. Zero
    /// disables it entirely, which is how the baseline pass in
    /// `tests/read_path_cost.rs` measures the cost of not having one.
    pub fn new(capacity: usize) -> Self {
        MetaCache {
            inner: Mutex::new(CacheInner {
                lru: NonZeroUsize::new(capacity).map(LruCache::new),
                hits: 0,
                misses: 0,
            }),
        }
    }

    /// Resize (or disable, at zero). Drops whatever was held, and
    /// restarts the counters: a hit rate averaged over two different
    /// capacities describes neither of them.
    pub fn set_capacity(&self, capacity: usize) {
        let mut g = self.inner.lock().expect("metadata cache lock");
        g.lru = NonZeroUsize::new(capacity).map(LruCache::new);
        g.hits = 0;
        g.misses = 0;
    }

    /// `(entries, capacity, hits, misses)`, for tests and diagnostics.
    pub fn stats(&self) -> (usize, usize, u64, u64) {
        let g = self.inner.lock().expect("metadata cache lock");
        let (entries, capacity) = match &g.lru {
            Some(lru) => (lru.len(), lru.cap().get()),
            None => (0, 0),
        };
        (entries, capacity, g.hits, g.misses)
    }

    /// A hit bumps the LRU recency; a miss is only counted when caching
    /// is actually live, so a disabled cache does not report a 0% hit
    /// rate over reads it was never asked about.
    fn get(&self, abs: u64) -> Option<CachedBlock> {
        let mut g = self.inner.lock().expect("metadata cache lock");
        let lru = g.lru.as_mut()?;
        match lru.get(&abs) {
            Some(hit) => {
                let hit = hit.clone();
                g.hits += 1;
                Some(hit)
            }
            None => {
                g.misses += 1;
                None
            }
        }
    }

    fn insert(&self, abs: u64, block: CachedBlock) {
        let mut g = self.inner.lock().expect("metadata cache lock");
        if let Some(lru) = g.lru.as_mut() {
            lru.put(abs, block);
        }
    }
}

/// Read + decompress one metadata block whose 2-byte header sits at
/// absolute byte offset `abs`. Returns the decompressed payload and the
/// absolute offset of the *next* metadata block.
///
/// With a `cache`, a block already decompressed comes back without
/// touching either the device or the codec.
pub fn read_block<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
    abs: u64,
    cache: Option<&MetaCache>,
) -> Result<(Arc<Vec<u8>>, u64)> {
    if let Some(hit) = cache.and_then(|c| c.get(abs)) {
        return Ok((hit.bytes, hit.next_abs));
    }
    let (bytes, next_abs) = read_block_uncached(dev, sb, abs)?;
    let block = CachedBlock {
        bytes: Arc::new(bytes),
        next_abs,
    };
    if let Some(cache) = cache {
        cache.insert(abs, block.clone());
    }
    Ok((block.bytes, next_abs))
}

/// The decompression itself, with no cache in front of it.
fn read_block_uncached<R: BlockRead + ?Sized>(
    dev: &R,
    sb: &Superblock,
    abs: u64,
) -> Result<(Vec<u8>, u64)> {
    let mut hdr = [0u8; 2];
    dev.read_at(abs, &mut hdr)?;
    let raw = u16::from_le_bytes(hdr);
    let on_disk = (raw & SIZE_MASK) as usize;
    let is_compressed = raw & UNCOMPRESSED_BIT == 0;
    if on_disk == 0 || on_disk > METADATA_SIZE {
        return Err(Error::BadMetadata("metadata block size out of range"));
    }

    let mut payload = vec![0u8; on_disk];
    dev.read_at(abs + 2, &mut payload)?;

    let out = if is_compressed {
        decompress::decompress(sb.compressor()?, &payload, METADATA_SIZE)?
    } else {
        // Uncompressed metadata blocks are stored verbatim, still capped
        // at the 8 KiB metadata size.
        if payload.len() > METADATA_SIZE {
            return Err(Error::BadMetadata("uncompressed metadata block too large"));
        }
        payload
    };
    Ok((out, abs + 2 + on_disk as u64))
}

/// A flat reader over a run of consecutive metadata blocks, starting at a
/// given absolute byte offset and in-block offset. Decompresses blocks
/// lazily as the caller reads past the current buffer.
/// A squashfs metadata reference: which metablock, and where inside it.
///
/// The 48/16 split was open-coded at three sites — `(r >> 16)` and
/// `(r & 0xFFFF)` written out each time, with the meaning of each half
/// carried only by a comment in a fourth place. Two shifts and a mask
/// is not hard to get right once; it is easy to get right differently
/// three times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataRef {
    /// Offset of the metablock from the start of its table.
    pub block_offset: u64,
    /// Byte offset of the record inside the decompressed metablock.
    pub in_block: u16,
}

impl MetadataRef {
    /// Split a packed reference.
    ///
    /// The high 48 bits are the block's offset from its table's start;
    /// the low 16 are the offset within the decompressed block. A
    /// metablock decompresses to at most 8 KiB, so 16 bits is generous
    /// and the split cannot lose information.
    pub fn from_packed(packed: u64) -> Self {
        MetadataRef {
            block_offset: packed >> 16,
            in_block: (packed & 0xFFFF) as u16,
        }
    }

    /// The absolute device offset of the metablock, given its table.
    ///
    /// Saturating, for the reason `Filesystem::block_offsets` already
    /// gives about its own running sum: `table_start` is a raw `u64`
    /// off the superblock and `block_offset` is 48 bits out of a
    /// reference, so the two can leave a `u64` between them. A plain
    /// `+` panicked in debug and wrapped in release, where this crate
    /// ships with `overflow-checks` off — and a wrapped offset reads
    /// some unrelated part of the image as the structure that was
    /// asked for. Measured with `inode_table_start` patched to
    /// 0xffff_ff00_0000_0060 and `root_inode_ref` to 1 << 56:
    ///
    /// ```text
    /// debug:   panicked at src/metablock.rs: attempt to add with overflow
    /// release: open ok, root err BadMetadata("metadata block size out of range")
    /// ```
    ///
    /// A panic in the profile the tests run and a quiet read from
    /// nowhere in the profile that ships is the worst pair available.
    /// Saturating gives an offset no device reaches, so the read fails
    /// and says so, in both.
    pub fn start_abs(self, table_start: u64) -> u64 {
        table_start.saturating_add(self.block_offset)
    }
}

/// The cursor's window onto decompressed metadata.
///
/// Nearly every cursor reads a few tens of bytes out of one 8 KiB block
/// and stops — an inode is small and a metablock is not. Such a cursor
/// borrows the cached block as it stands. Copying it into a private
/// buffer first would put an 8 KiB memcpy in front of every inode read,
/// which is exactly the kind of per-access cost this cache exists to
/// remove; it would not be visible in the device-read column and would
/// quietly eat part of the win in the wall-clock one.
///
/// Only a record straddling a block boundary needs bytes of its own, and
/// that is what [`Window::Spliced`] is for.
enum Window {
    /// One cached block, shared rather than copied.
    Whole(Arc<Vec<u8>>),
    /// The tail of one block followed by whole blocks after it.
    Spliced(Vec<u8>),
}

impl Window {
    fn bytes(&self) -> &[u8] {
        match self {
            Window::Whole(b) => b,
            Window::Spliced(b) => b,
        }
    }
}

pub struct MetaCursor<'a, R: BlockRead + ?Sized> {
    dev: &'a R,
    sb: &'a Superblock,
    cache: Option<&'a MetaCache>,
    /// Absolute offset of the NEXT metadata block to pull.
    next_abs: u64,
    /// Decompressed bytes accumulated so far, minus what's been consumed.
    buf: Window,
    /// Read cursor within `buf`.
    pos: usize,
}

impl<'a, R: BlockRead + ?Sized> MetaCursor<'a, R> {
    /// Start a cursor at the metadata block beginning at absolute offset
    /// `start_abs`, positioned `in_block` bytes into that decompressed
    /// block (this is exactly how a SquashFS metadata reference decodes:
    /// `start_abs = table_start + (ref >> 16)`, `in_block = ref & 0xFFFF`).
    ///
    /// `cache`, when given, is consulted here and on every refill. This
    /// is the one place worth putting it: every metadata read in the
    /// crate — inodes, directory listings — arrives through this
    /// constructor.
    pub fn new(
        dev: &'a R,
        sb: &'a Superblock,
        start_abs: u64,
        in_block: u16,
        cache: Option<&'a MetaCache>,
    ) -> Result<Self> {
        let (block, next) = read_block(dev, sb, start_abs, cache)?;
        if in_block as usize > block.len() {
            return Err(Error::BadMetadata(
                "metadata reference offset past block end",
            ));
        }
        Ok(MetaCursor {
            dev,
            sb,
            cache,
            next_abs: next,
            buf: Window::Whole(block),
            pos: in_block as usize,
        })
    }

    /// Pull one more metadata block onto the tail of `buf`.
    fn refill(&mut self) -> Result<()> {
        let (block, next) = read_block(self.dev, self.sb, self.next_abs, self.cache)?;
        if block.is_empty() {
            return Err(Error::BadMetadata(
                "empty metadata block while reading record",
            ));
        }
        // Drop already-consumed bytes to keep the buffer bounded, then
        // append the freshly-decompressed block. Crossing a boundary is
        // where the cursor stops being able to share a cached block, so
        // this is also where the window becomes its own.
        let mut spliced = match std::mem::replace(&mut self.buf, Window::Spliced(Vec::new())) {
            Window::Whole(b) => b[self.pos..].to_vec(),
            Window::Spliced(mut b) => {
                b.drain(..self.pos);
                b
            }
        };
        self.pos = 0;
        spliced.extend_from_slice(&block);
        self.buf = Window::Spliced(spliced);
        self.next_abs = next;
        Ok(())
    }

    /// Ensure at least `n` unread bytes are available, pulling blocks as
    /// needed.
    fn ensure(&mut self, n: usize) -> Result<()> {
        while self.buf.bytes().len() - self.pos < n {
            self.refill()?;
        }
        Ok(())
    }

    /// Read exactly `n` bytes, advancing the cursor.
    pub fn read_exact(&mut self, n: usize) -> Result<Vec<u8>> {
        self.ensure(n)?;
        let out = self.buf.bytes()[self.pos..self.pos + n].to_vec();
        self.pos += n;
        Ok(out)
    }

    pub fn read_u16(&mut self) -> Result<u16> {
        let b = self.read_exact(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn read_i16(&mut self) -> Result<i16> {
        Ok(self.read_u16()? as i16)
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        let b = self.read_exact(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_u64(&mut self) -> Result<u64> {
        let b = self.read_exact(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::superblock::tests::synth_sb;
    use crate::superblock::Superblock;
    use flate2::{Compress, Compression, FlushCompress};
    use std::sync::Mutex;

    pub(crate) struct MemDev(pub Mutex<Vec<u8>>);
    impl BlockRead for MemDev {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let v = self.0.lock().unwrap();
            let start = offset as usize;
            // Saturating, for the same reason the crate saturates: a
            // test that feeds this an offset the crate has just refused
            // — `u64::MAX` — must get the short read back rather than
            // panic inside the double. Its twin in tests/common/mod.rs
            // had the same `+` and the same problem.
            let end = start.saturating_add(buf.len());
            if start > v.len() || end > v.len() {
                return Err(fs_core::Error::ShortRead {
                    offset,
                    want: buf.len(),
                    got: v.len().saturating_sub(start),
                });
            }
            buf.copy_from_slice(&v[start..end]);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.0.lock().unwrap().len() as u64
        }
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut enc = Compress::new(Compression::default(), true);
        let mut out = vec![0u8; data.len() + 64];
        enc.compress(data, &mut out, FlushCompress::Finish).unwrap();
        out.truncate(enc.total_out() as usize);
        out
    }

    /// Emit a metadata block (header + payload) for `payload`, compressed.
    pub(crate) fn emit_meta(payload: &[u8]) -> Vec<u8> {
        let comp = zlib(payload);
        let mut out = Vec::new();
        out.extend_from_slice(&(comp.len() as u16).to_le_bytes());
        out.extend_from_slice(&comp);
        out
    }

    /// Emit an UNCOMPRESSED metadata block (sets bit 15).
    fn emit_meta_raw(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&((payload.len() as u16) | UNCOMPRESSED_BIT).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn sb() -> Superblock {
        Superblock::parse(&synth_sb(17, 0, 0)).unwrap()
    }

    #[test]
    fn read_compressed_block() {
        let payload = b"the quick brown fox".repeat(20);
        let img = emit_meta(&payload);
        let dev = MemDev(Mutex::new(img.clone()));
        let (out, next) = read_block(&dev, &sb(), 0, None).unwrap();
        assert_eq!(*out, payload);
        assert_eq!(next as usize, img.len());
    }

    #[test]
    fn read_uncompressed_block() {
        let payload = b"raw bytes";
        let img = emit_meta_raw(payload);
        let dev = MemDev(Mutex::new(img));
        let (out, _next) = read_block(&dev, &sb(), 0, None).unwrap();
        assert_eq!(out.as_slice(), payload);
    }

    #[test]
    fn cursor_spans_block_boundary() {
        // Two metadata blocks; a record straddles the seam.
        let first = vec![0xAAu8; 100];
        let second = {
            let mut v = vec![0xBBu8; 100];
            v[0] = 0xCC;
            v
        };
        let mut img = emit_meta(&first);
        img.extend_from_slice(&emit_meta(&second));
        let dev = MemDev(Mutex::new(img));
        let s = sb();
        let mut cur = MetaCursor::new(&dev, &s, 0, 98, None).unwrap();
        // Read 4 bytes starting 98 into block 0: 2 from block 0 (0xAA),
        // then 2 from block 1 (0xCC, 0xBB).
        let got = cur.read_exact(4).unwrap();
        assert_eq!(got, vec![0xAA, 0xAA, 0xCC, 0xBB]);
    }

    #[test]
    fn rejects_zero_size_header() {
        let img = vec![0u8, 0u8]; // size 0
        let dev = MemDev(Mutex::new(img));
        assert!(matches!(
            read_block(&dev, &sb(), 0, None),
            Err(Error::BadMetadata(_))
        ));
    }

    /// A device that counts what it is asked for, so a test can say
    /// "and this time nothing reached the disk" rather than inferring it.
    struct CountingMem {
        inner: MemDev,
        reads: Mutex<u64>,
    }

    impl CountingMem {
        fn new(bytes: Vec<u8>) -> Self {
            CountingMem {
                inner: MemDev(Mutex::new(bytes)),
                reads: Mutex::new(0),
            }
        }
        fn reads(&self) -> u64 {
            *self.reads.lock().unwrap()
        }
    }

    impl BlockRead for CountingMem {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            *self.reads.lock().unwrap() += 1;
            self.inner.read_at(offset, buf)
        }
        fn size_bytes(&self) -> u64 {
            self.inner.size_bytes()
        }
    }

    /// Two metadata blocks back to back, and where the second starts.
    fn two_blocks() -> (Vec<u8>, Vec<u8>, Vec<u8>, u64) {
        let first = b"first block payload".repeat(11);
        let second = b"second block payload".repeat(13);
        let mut img = emit_meta(&first);
        let second_abs = img.len() as u64;
        img.extend_from_slice(&emit_meta(&second));
        (img, first, second, second_abs)
    }

    #[test]
    fn a_second_read_of_the_same_block_touches_neither_device_nor_codec() {
        let (img, first, _second, _) = two_blocks();
        let dev = CountingMem::new(img);
        let cache = MetaCache::new(8);

        let (a, next_a) = read_block(&dev, &sb(), 0, Some(&cache)).unwrap();
        let after_first = dev.reads();
        assert!(after_first > 0, "the first read must reach the device");

        let (b, next_b) = read_block(&dev, &sb(), 0, Some(&cache)).unwrap();
        assert_eq!(
            dev.reads(),
            after_first,
            "the second read reached the device"
        );
        assert_eq!(*b, first, "the cached bytes are not the block's bytes");
        // The successor offset is NOT derivable from the decompressed
        // bytes -- it comes from the on-disk length in the 2-byte header,
        // which a hit never reads. Getting it wrong would send the next
        // refill to the wrong place, and only a cursor spanning a
        // boundary would ever notice.
        assert_eq!(next_b, next_a, "the cached successor offset is wrong");
        // Shared, not copied: a hit hands out the same allocation.
        assert!(Arc::ptr_eq(&a, &b));

        let (_, _, hits, misses) = cache.stats();
        assert_eq!((hits, misses), (1, 1));
    }

    #[test]
    fn a_disabled_cache_decompresses_every_time() {
        let (img, first, _second, _) = two_blocks();
        let dev = CountingMem::new(img);
        let cache = MetaCache::new(0);

        let (a, _) = read_block(&dev, &sb(), 0, Some(&cache)).unwrap();
        let after_first = dev.reads();
        let (b, _) = read_block(&dev, &sb(), 0, Some(&cache)).unwrap();

        assert_eq!(*a, first);
        assert_eq!(*b, first);
        assert!(dev.reads() > after_first, "a disabled cache served a hit");
        // A disabled cache reports nothing rather than a 0% hit rate over
        // reads it was never consulted about.
        assert_eq!(cache.stats(), (0, 0, 0, 0));
    }

    #[test]
    fn the_cache_evicts_at_capacity() {
        let (img, _first, _second, second_abs) = two_blocks();
        let dev = CountingMem::new(img);
        let cache = MetaCache::new(1);

        // Alternating between two blocks with room for one: the entry
        // put in by each read is the one the next read evicts, so every
        // read is a miss.
        for _ in 0..3 {
            read_block(&dev, &sb(), 0, Some(&cache)).unwrap();
            read_block(&dev, &sb(), second_abs, Some(&cache)).unwrap();
        }
        let (entries, capacity, hits, misses) = cache.stats();
        assert_eq!((entries, capacity), (1, 1));
        assert_eq!(hits, 0, "an entry survived past the capacity");
        assert_eq!(misses, 6);
    }

    #[test]
    fn set_capacity_to_zero_switches_it_off_and_drops_what_it_held() {
        let (img, _first, _second, _) = two_blocks();
        let dev = CountingMem::new(img);
        let cache = MetaCache::new(8);
        read_block(&dev, &sb(), 0, Some(&cache)).unwrap();
        assert_eq!(cache.stats().0, 1);

        // Resizing restarts the counters as well as dropping the
        // entries: a hit rate averaged over two capacities is a number
        // about neither of them.
        cache.set_capacity(0);
        assert_eq!(cache.stats(), (0, 0, 0, 0));
        let before = dev.reads();
        read_block(&dev, &sb(), 0, Some(&cache)).unwrap();
        assert!(dev.reads() > before, "a switched-off cache still served");
    }

    /// The cache changes how the cursor holds its bytes -- one block is
    /// borrowed from the cache, a record spanning two gets a buffer of
    /// its own -- so the spanning case has to give the same answer with
    /// the cache on as with it off.
    #[test]
    fn a_record_spanning_a_boundary_reads_the_same_cached_or_not() {
        let first = vec![0xAAu8; 100];
        let second = {
            let mut v = vec![0xBBu8; 100];
            v[0] = 0xCC;
            v
        };
        let mut img = emit_meta(&first);
        img.extend_from_slice(&emit_meta(&second));
        let dev = MemDev(Mutex::new(img));
        let s = sb();
        let cache = MetaCache::new(8);

        let mut cold = MetaCursor::new(&dev, &s, 0, 98, Some(&cache)).unwrap();
        let cold = cold.read_exact(4).unwrap();
        // Second time round both blocks are already in the cache, which
        // is the path where the cursor splices a borrowed block onto a
        // fresh one.
        let mut warm = MetaCursor::new(&dev, &s, 0, 98, Some(&cache)).unwrap();
        let warm = warm.read_exact(4).unwrap();

        assert_eq!(cold, vec![0xAA, 0xAA, 0xCC, 0xBB]);
        assert_eq!(warm, cold);
    }

    /// Reading a long record pulls block after block. Each one must be
    /// spliced on whole: an off-by-one in the drain would corrupt the
    /// seam, and a short record would never notice.
    #[test]
    fn a_record_spanning_three_blocks_comes_back_byte_for_byte() {
        let blocks: Vec<Vec<u8>> = (0u8..3).map(|i| vec![0x10 + i; 200]).collect();
        let mut img = Vec::new();
        for b in &blocks {
            img.extend_from_slice(&emit_meta(b));
        }
        let dev = MemDev(Mutex::new(img));
        let s = sb();
        let cache = MetaCache::new(8);

        let want: Vec<u8> = blocks.concat()[50..].to_vec();
        for pass in 0..2 {
            let mut cur = MetaCursor::new(&dev, &s, 0, 50, Some(&cache)).unwrap();
            let got = cur.read_exact(want.len()).unwrap();
            assert_eq!(got, want, "pass {pass}");
        }
    }
}

#[cfg(test)]
mod metadata_ref_tests {
    use super::MetadataRef;

    /// The 48/16 split, against literal bits.
    ///
    /// It was open-coded at three sites with the meaning of each half
    /// carried only by a comment in a fourth. Two shifts and a mask is
    /// not hard to get right once; it is easy to get right *differently*
    /// three times.
    #[test]
    fn the_low_sixteen_bits_are_the_offset_within_the_block() {
        let r = MetadataRef::from_packed(0x0000_0001_2345_ABCD);
        assert_eq!(r.block_offset, 0x0000_0001_2345, "the high 48 bits");
        assert_eq!(r.in_block, 0xABCD, "the low 16 bits");
    }

    #[test]
    fn a_reference_at_the_start_of_the_first_block_is_all_zeroes() {
        let r = MetadataRef::from_packed(0);
        assert_eq!((r.block_offset, r.in_block), (0, 0));
        assert_eq!(r.start_abs(4096), 4096, "the table's own start");
    }

    /// The sum saturates rather than wrapping.
    ///
    /// `table_start` comes off the superblock and `block_offset` is 48
    /// bits out of a reference, so between them they can leave a `u64`.
    /// A plain `+` panicked in debug and wrapped in release, where this
    /// crate ships with `overflow-checks` off — and a wrapped offset
    /// names real bytes, so the read succeeds against the wrong part of
    /// the image. `u64::MAX` is an offset no device reaches, so the
    /// read fails and says where.
    ///
    /// This covers the directory listing too: `Filesystem::read_dir`
    /// goes through this function rather than writing the sum out
    /// again, which is how the two came to disagree in the first place.
    #[test]
    fn start_abs_saturates_rather_than_wrapping() {
        // packed >> 16 is the block offset, so 1 << 56 packed is
        // 1 << 40 offset — the value the issue reports.
        let r = MetadataRef::from_packed(1u64 << 56);
        assert_eq!(r.block_offset, 1 << 40);
        assert_eq!(r.start_abs(0xffff_ff00_0000_0060), u64::MAX);
        // And an ordinary reference is untouched by the change.
        let r = MetadataRef::from_packed((3u64 << 16) | 7);
        assert_eq!(r.start_abs(1000), 1003);
    }

    /// `start_abs` is the table start plus the block offset — never the
    /// packed value, which is the mistake the split exists to prevent.
    #[test]
    fn start_abs_adds_the_block_offset_to_the_table() {
        let r = MetadataRef::from_packed((7 << 16) | 42);
        assert_eq!(r.block_offset, 7);
        assert_eq!(r.in_block, 42);
        assert_eq!(r.start_abs(1000), 1007);
    }

    /// The widest offset the low half can hold is inside a metablock,
    /// which decompresses to at most 8 KiB — so 16 bits is generous and
    /// the split cannot lose information.
    #[test]
    fn sixteen_bits_more_than_covers_a_metablock() {
        assert!(u16::MAX as usize >= 8192, "a metablock is at most 8 KiB");
    }
}
