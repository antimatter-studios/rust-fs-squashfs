//! SquashFS superblock parsing.
//!
//! The superblock lives at byte offset 0 and is 96 bytes long. Field
//! layout matches `struct squashfs_super_block` in squashfs-tools'
//! `squashfs_fs.h`; all fields are little-endian. Names mirror the C
//! struct with the redundant prefixes dropped.

use crate::damage::{Damage, DamageKind, Severity};
use crate::decompress::Compressor;
use crate::error::{Error, Result};
use fs_core::BlockRead;

/// "hsqs" little-endian — the SquashFS 4.0 magic at offset 0.
pub const SQUASHFS_MAGIC: u32 = 0x7371_7368;
pub const SUPERBLOCK_SIZE: usize = 96;

/// SquashFS metadata blocks are a fixed 8 KiB when decompressed.
pub const METADATA_SIZE: usize = 8192;

/// The one minor version of SquashFS 4 there is (`SQUASHFS_MINOR`).
pub const SQUASHFS_MINOR: u16 = 0;

/// Block-size bounds. SquashFS supports 4 KiB .. 1 MiB data blocks
/// (block_log 12..=20). Reject anything outside to catch corruption.
/// The smallest an inode can be on disk: `squashfs_base_inode_header`,
/// the 16-byte common header every inode type begins with. Used to turn
/// the inode table's extent into a bound on `inode_count`.
const MIN_INODE_SIZE: usize = 16;

const MIN_BLOCK_LOG: u16 = 12;
const MAX_BLOCK_LOG: u16 = 20;

/// `fragment_block_index` sentinel: this file has no tail fragment.
pub const SQUASHFS_INVALID_FRAG: u32 = 0xFFFF_FFFF;

#[derive(Debug, Clone)]
pub struct Superblock {
    pub magic: u32,
    pub inode_count: u32,
    pub modification_time: u32,
    pub block_size: u32,
    pub fragment_entry_count: u32,
    pub compression_id: u16,
    pub block_log: u16,
    pub flags: u16,
    pub id_count: u16,
    pub version_major: u16,
    pub version_minor: u16,
    /// Metadata reference to the root inode: high 48 bits = byte offset of
    /// its metadata block relative to `inode_table_start`; low 16 bits =
    /// offset within the decompressed block.
    pub root_inode_ref: u64,
    pub bytes_used: u64,
    pub id_table_start: u64,
    pub xattr_id_table_start: u64,
    pub inode_table_start: u64,
    pub directory_table_start: u64,
    pub fragment_table_start: u64,
    pub export_table_start: u64,
}

/// Byte offset and width of each superblock field a fault can name, so
/// a report points at the bytes and not only at a name.
mod field {
    pub const MAGIC: (u64, u64) = (0x00, 4);
    pub const INODE_COUNT: (u64, u64) = (0x04, 4);
    pub const BLOCK_SIZE: (u64, u64) = (0x0C, 4);
    pub const COMPRESSION_ID: (u64, u64) = (0x14, 2);
    pub const BLOCK_LOG: (u64, u64) = (0x16, 2);
    pub const VERSION_MAJOR: (u64, u64) = (0x1C, 2);
    pub const VERSION_MINOR: (u64, u64) = (0x1E, 2);
    pub const ROOT_INODE_REF: (u64, u64) = (0x20, 8);
    pub const BYTES_USED: (u64, u64) = (0x28, 8);
    pub const ID_TABLE_START: (u64, u64) = (0x30, 8);
    pub const XATTR_ID_TABLE_START: (u64, u64) = (0x38, 8);
    pub const INODE_TABLE_START: (u64, u64) = (0x40, 8);
    pub const DIRECTORY_TABLE_START: (u64, u64) = (0x48, 8);
    pub const FRAGMENT_TABLE_START: (u64, u64) = (0x50, 8);
    pub const EXPORT_TABLE_START: (u64, u64) = (0x58, 8);
}

/// Which superblock rule a field broke.
///
/// Each one is the strict parse's reason for refusing the superblock, so
/// [`SuperblockFault::to_error`] gives back exactly the error the strict
/// reader returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperblockRule {
    /// The magic at offset 0 is not "hsqs".
    Magic,
    /// The major version is not 4, or the minor is later than 4.0.
    Version,
    /// `block_log` is outside 12..=20, or `block_size` is not `1 << block_log`.
    BlockSize,
    /// The compression id names no SquashFS compressor.
    Compressor,
    /// `bytes_used` is larger than the device.
    BytesUsed,
    /// A table starts outside `[96, bytes_used)`.
    TableBounds,
    /// The table starts are not in the order `mksquashfs` writes them.
    TableOrder,
    /// The root inode's metadata block is outside the inode table.
    RootInode,
    /// `inode_count` is more than the inode table has room for.
    InodeCount,
}

/// One superblock field the strict parse would refuse: which field, where
/// its bytes are, and why.
///
/// A fault is evidence, not an error: the field's raw value is still in
/// the decoded [`Superblock`], and the other fields are still usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuperblockFault {
    /// The broken rule.
    pub rule: SuperblockRule,
    /// The field's name, as in `struct squashfs_super_block`.
    pub field: &'static str,
    /// Byte offset of the field in the image (the superblock is at 0).
    pub offset: u64,
    /// Width of the field in bytes.
    pub length: u64,
    /// Why the field is wrong, in the strict parse's words.
    pub reason: &'static str,
}

impl SuperblockFault {
    fn new(
        rule: SuperblockRule,
        field: &'static str,
        at: (u64, u64),
        reason: &'static str,
    ) -> Self {
        SuperblockFault {
            rule,
            field,
            offset: at.0,
            length: at.1,
            reason,
        }
    }

    /// The error the strict reader returns for this fault.
    pub fn to_error(&self, sb: &Superblock) -> Error {
        match self.rule {
            SuperblockRule::Magic => Error::NotSquashfs,
            SuperblockRule::Compressor => Error::UnsupportedCompression(sb.compression_id),
            _ => Error::BadSuperblock(self.reason),
        }
    }

    /// The fault as damage in the shared vocabulary.
    pub fn damage(&self) -> Damage {
        let kind = match self.rule {
            SuperblockRule::Magic => DamageKind::Signature,
            SuperblockRule::TableOrder
            | SuperblockRule::RootInode
            | SuperblockRule::InodeCount
            | SuperblockRule::BytesUsed => DamageKind::Mismatch,
            _ => DamageKind::Malformed,
        };
        Damage {
            kind,
            severity: Severity::Error,
            offset: self.offset,
            length: self.length,
            structure: "superblock",
            detail: format!("{}: {}", self.field, self.reason),
        }
    }
}

impl Superblock {
    /// Decode the 96 bytes as they stand, judging nothing.
    ///
    /// The primitive under [`Superblock::parse`]: every field comes back
    /// with whatever value the bytes hold, however wrong, so a damaged
    /// superblock still shows its other 90 bytes. Pair it with
    /// [`Superblock::faults`] to learn which fields the strict parse
    /// would refuse.
    pub fn decode(b: &[u8; SUPERBLOCK_SIZE]) -> Self {
        let rd_u16 = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let rd_u32 = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let rd_u64 = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        Superblock {
            magic: rd_u32(0x00),
            inode_count: rd_u32(0x04),
            modification_time: rd_u32(0x08),
            block_size: rd_u32(0x0C),
            fragment_entry_count: rd_u32(0x10),
            compression_id: rd_u16(0x14),
            block_log: rd_u16(0x16),
            flags: rd_u16(0x18),
            id_count: rd_u16(0x1A),
            version_major: rd_u16(0x1C),
            version_minor: rd_u16(0x1E),
            root_inode_ref: rd_u64(0x20),
            bytes_used: rd_u64(0x28),
            id_table_start: rd_u64(0x30),
            xattr_id_table_start: rd_u64(0x38),
            inode_table_start: rd_u64(0x40),
            directory_table_start: rd_u64(0x48),
            fragment_table_start: rd_u64(0x50),
            export_table_start: rd_u64(0x58),
        }
    }

    /// Parse + validate a superblock from a 96-byte buffer.
    ///
    /// [`Superblock::decode`], refused on the first of
    /// [`Superblock::header_faults`].
    pub fn parse(b: &[u8]) -> Result<Self> {
        let Some(b) = b.get(..SUPERBLOCK_SIZE) else {
            return Err(Error::BadSuperblock("buffer shorter than 96 bytes"));
        };
        let sb = Self::decode(b.try_into().expect("96 bytes"));
        match sb.header_faults().first() {
            Some(fault) => Err(fault.to_error(&sb)),
            None => Ok(sb),
        }
    }

    /// Every fault in the superblock on its own: the magic, the version
    /// and the block size, in the order [`Superblock::parse`] checks
    /// them. Empty when the parse accepts it.
    pub fn header_faults(&self) -> Vec<SuperblockFault> {
        use SuperblockRule::*;
        let mut out = Vec::new();
        if self.magic != SQUASHFS_MAGIC {
            out.push(SuperblockFault::new(
                Magic,
                "magic",
                field::MAGIC,
                "not the SquashFS magic",
            ));
        }
        // The version before the block size: a 3.x superblock shares the
        // magic and the major's offset, but not `block_log`'s, and was
        // refused as a bad block size rather than as the version it is.
        if self.version_major != 4 {
            out.push(SuperblockFault::new(
                Version,
                "version_major",
                field::VERSION_MAJOR,
                "only SquashFS 4.x is supported",
            ));
        }
        // As the kernel does (`s_minor > SQUASHFS_MINOR`): a later minor
        // may change the layout this parser assumes.
        if self.version_minor > SQUASHFS_MINOR {
            out.push(SuperblockFault::new(
                Version,
                "version_minor",
                field::VERSION_MINOR,
                "unknown SquashFS minor version (only 4.0 is supported)",
            ));
        }
        if !(MIN_BLOCK_LOG..=MAX_BLOCK_LOG).contains(&self.block_log) {
            out.push(SuperblockFault::new(
                BlockSize,
                "block_log",
                field::BLOCK_LOG,
                "block_log out of range",
            ));
        } else if self.block_size != 1u32 << self.block_log {
            out.push(SuperblockFault::new(
                BlockSize,
                "block_size",
                field::BLOCK_SIZE,
                "block_size != 1 << block_log",
            ));
        }
        out
    }

    /// The compression id, when it names no SquashFS compressor.
    pub fn compressor_fault(&self) -> Option<SuperblockFault> {
        Compressor::from_id(self.compression_id).err().map(|_| {
            SuperblockFault::new(
                SuperblockRule::Compressor,
                "compression_id",
                field::COMPRESSION_ID,
                "unknown compressor id",
            )
        })
    }

    /// Every fault there is: the header's, the compressor's, and the
    /// device's ([`Superblock::device_faults`]), in that order -- the
    /// order the strict open meets them. Empty when it would accept the
    /// superblock.
    pub fn faults(&self, device_size: u64) -> Vec<SuperblockFault> {
        let mut out = self.header_faults();
        out.extend(self.compressor_fault());
        out.extend(self.device_faults(device_size));
        out
    }

    /// Check the superblock's sizes and table starts against the device
    /// it was read from.
    ///
    /// Refuses on the first of [`Superblock::device_faults`].
    pub fn validate_against_device(&self, device_size: u64) -> Result<()> {
        match self.device_faults(device_size).first() {
            Some(fault) => Err(fault.to_error(self)),
            None => Ok(()),
        }
    }

    /// Every fault in the superblock's sizes and table starts, measured
    /// against the device it was read from, in the order
    /// [`Superblock::validate_against_device`] checks them.
    ///
    /// The header alone sees 96 bytes and nothing else, so everything
    /// here was taken on trust: a header claiming `bytes_used` = 1 TiB
    /// over a 20 KiB file mounted and published a terabyte through
    /// `fs_squashfs_get_volume_info`, and table starts past the end,
    /// inside the superblock or out of order opened cleanly and failed
    /// later as a short read at an absurd offset, if at all (#48).
    ///
    /// The layout `mksquashfs` writes, and the kernel's
    /// `squashfs_fill_super` relies on, is the inode table, then the
    /// directory table, then the fragment, export, id and xattr tables'
    /// index arrays in that order, each preceded by its own metadata, all
    /// below `bytes_used`. Absent optional tables carry [`NO_TABLE`] and
    /// are left out of the ordering.
    ///
    /// [`NO_TABLE`]: crate::table::NO_TABLE
    pub fn device_faults(&self, device_size: u64) -> Vec<SuperblockFault> {
        use crate::table::NO_TABLE;
        use SuperblockRule::*;
        let mut out = Vec::new();
        if self.bytes_used > device_size {
            out.push(SuperblockFault::new(
                BytesUsed,
                "bytes_used",
                field::BYTES_USED,
                "bytes_used exceeds the size of the device",
            ));
        }
        let start = SUPERBLOCK_SIZE as u64;
        let in_image = |t: u64| (start..self.bytes_used).contains(&t);
        const OUTSIDE: &str = "a table starts outside [superblock end, bytes_used)";
        // Required tables.
        for (t, name, at) in [
            (
                self.inode_table_start,
                "inode_table_start",
                field::INODE_TABLE_START,
            ),
            (
                self.directory_table_start,
                "directory_table_start",
                field::DIRECTORY_TABLE_START,
            ),
            (self.id_table_start, "id_table_start", field::ID_TABLE_START),
        ] {
            if !in_image(t) {
                out.push(SuperblockFault::new(TableBounds, name, at, OUTSIDE));
            }
        }
        // Optional tables: absent, or inside the image.
        for (t, name, at) in [
            (
                self.fragment_table_start,
                "fragment_table_start",
                field::FRAGMENT_TABLE_START,
            ),
            (
                self.export_table_start,
                "export_table_start",
                field::EXPORT_TABLE_START,
            ),
            (
                self.xattr_id_table_start,
                "xattr_id_table_start",
                field::XATTR_ID_TABLE_START,
            ),
        ] {
            if t != NO_TABLE && !in_image(t) {
                out.push(SuperblockFault::new(TableBounds, name, at, OUTSIDE));
            }
        }
        // The inode table is strictly before the directory table (the
        // root inode lives in it); every later table starts at or after
        // the one before it.
        if self.inode_table_start >= self.directory_table_start {
            out.push(SuperblockFault::new(
                TableOrder,
                "directory_table_start",
                field::DIRECTORY_TABLE_START,
                "table starts are out of order (inode table not before directory table)",
            ));
        }
        let mut prev = self.directory_table_start;
        for (t, name, at) in [
            (
                self.fragment_table_start,
                "fragment_table_start",
                field::FRAGMENT_TABLE_START,
            ),
            (
                self.export_table_start,
                "export_table_start",
                field::EXPORT_TABLE_START,
            ),
            (self.id_table_start, "id_table_start", field::ID_TABLE_START),
            (
                self.xattr_id_table_start,
                "xattr_id_table_start",
                field::XATTR_ID_TABLE_START,
            ),
        ] {
            if t == NO_TABLE {
                continue;
            }
            if t < prev {
                out.push(SuperblockFault::new(
                    TableOrder,
                    name,
                    at,
                    "table starts are out of order",
                ));
            }
            prev = t;
        }
        // The root inode's metadata block starts inside the inode table.
        let root_block = self.root_inode_ref >> 16;
        if self
            .inode_table_start
            .checked_add(root_block)
            .is_none_or(|b| b >= self.directory_table_start)
        {
            out.push(SuperblockFault::new(
                RootInode,
                "root_inode_ref",
                field::ROOT_INODE_REF,
                "root inode reference points outside the inode table",
            ));
        }
        // `inode_count` against the room the inode table actually has.
        //
        // Nothing else bounds it on every image. The export table reader
        // checks the count against the pointer array it sizes, and says
        // so in its own comment -- but `mksquashfs -no-exports` is a
        // normal option, common in firmware, and then
        // `read_export_table` returns before that check is reached. So
        // the one image shape where the value is attacker-controlled was
        // the one shape nothing bounded it on, and it crosses the C ABI
        // as the image declared it (#106).
        //
        // THE OBVIOUS BOUND -- `inode_count * 16 <= the table's extent`
        // -- IS WRONG AND WOULD REFUSE ORDINARY IMAGES. The inode table
        // is compressed metadata: a metablock holding 8192 bytes of
        // inodes can be a few hundred bytes on disk, so the extent says
        // nothing directly about how many inodes are in it.
        //
        // What the extent does bound is the number of METABLOCKS. Each
        // is a 2-byte header and at least one byte of payload
        // (`metablock::read` refuses `on_disk == 0`), so an extent of E
        // bytes holds at most E/3 of them; each decompresses to at most
        // `METADATA_SIZE`; and the smallest inode on disk is the 16-byte
        // common header. That is a generous bound -- no real image comes
        // near it, because no real metablock compresses to three bytes
        // -- which is the point: it refuses only counts the table could
        // not hold however well it compressed.
        let extent = self
            .inode_table_end()
            .saturating_sub(self.inode_table_start);
        let room = (extent / 3).saturating_mul((METADATA_SIZE / MIN_INODE_SIZE) as u64);
        if u64::from(self.inode_count) > room {
            out.push(SuperblockFault::new(
                InodeCount,
                "inode_count",
                field::INODE_COUNT,
                "inode_count exceeds what the inode table has room for",
            ));
        }
        out
    }

    /// Where the inode table ends, as far as the superblock can say.
    ///
    /// The same shape as [`directory_table_end`], and for the same
    /// reason: the nearest later table start, or `bytes_used`, is an
    /// upper bound on this table's last byte. Unlike that one this
    /// cannot be `u64::MAX` after `validate_against_device` has checked
    /// the ordering -- `bytes_used` is always in the list and the inode
    /// table always starts below it -- but the `unwrap_or` is kept so
    /// the function is honest when called on its own.
    ///
    /// [`directory_table_end`]: Self::directory_table_end
    pub fn inode_table_end(&self) -> u64 {
        [
            self.directory_table_start,
            self.fragment_table_start,
            self.export_table_start,
            self.id_table_start,
            self.xattr_id_table_start,
            self.bytes_used,
        ]
        .into_iter()
        .filter(|&t| t != crate::table::NO_TABLE && t > self.inode_table_start)
        .min()
        .unwrap_or(u64::MAX)
    }

    /// Where the directory table ends, as far as the superblock can say.
    ///
    /// The tables after it (fragment, export, id, xattr) each start with
    /// their own metadata blocks and then the index array their `_start`
    /// names, so the nearest of those starts -- or `bytes_used` -- is an
    /// upper bound on the directory table's last byte. `u64::MAX` when
    /// nothing after it is known (a superblock that names no later table
    /// and no size), which leaves the old behaviour in place.
    pub fn directory_table_end(&self) -> u64 {
        [
            self.fragment_table_start,
            self.export_table_start,
            self.id_table_start,
            self.xattr_id_table_start,
            self.bytes_used,
        ]
        .into_iter()
        .filter(|&t| t != crate::table::NO_TABLE && t > self.directory_table_start)
        .min()
        .unwrap_or(u64::MAX)
    }

    /// Resolve the archive-wide compressor. Returns
    /// [`Error::UnsupportedCompression`] for ids we don't recognise.
    pub fn compressor(&self) -> Result<Compressor> {
        Compressor::from_id(self.compression_id)
    }
}

/// Read + parse the superblock from a block device.
pub fn read<R: BlockRead + ?Sized>(dev: &R) -> Result<Superblock> {
    let mut buf = [0u8; SUPERBLOCK_SIZE];
    dev.read_at(0, &mut buf)?;
    Superblock::parse(&buf)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a minimal valid 96-byte superblock for unit tests.
    pub(crate) fn synth_sb(block_log: u16, root_ref: u64, inode_table_start: u64) -> Vec<u8> {
        let mut b = vec![0u8; SUPERBLOCK_SIZE];
        b[0x00..0x04].copy_from_slice(&SQUASHFS_MAGIC.to_le_bytes());
        b[0x0C..0x10].copy_from_slice(&(1u32 << block_log).to_le_bytes());
        b[0x14..0x16].copy_from_slice(&1u16.to_le_bytes()); // gzip
        b[0x16..0x18].copy_from_slice(&block_log.to_le_bytes());
        b[0x1C..0x1E].copy_from_slice(&4u16.to_le_bytes()); // version_major
        b[0x20..0x28].copy_from_slice(&root_ref.to_le_bytes());
        b[0x40..0x48].copy_from_slice(&inode_table_start.to_le_bytes());
        b
    }

    #[test]
    fn parse_minimal() {
        let buf = synth_sb(17, 0, 96);
        let sb = Superblock::parse(&buf).unwrap();
        assert_eq!(sb.magic, SQUASHFS_MAGIC);
        assert_eq!(sb.block_size, 1 << 17);
        assert_eq!(sb.block_log, 17);
        assert_eq!(sb.version_major, 4);
        assert_eq!(sb.compressor().unwrap(), Compressor::Gzip);
        assert_eq!(sb.inode_table_start, 96);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = synth_sb(17, 0, 96);
        buf[0..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(matches!(Superblock::parse(&buf), Err(Error::NotSquashfs)));
    }

    #[test]
    fn rejects_block_size_mismatch() {
        let mut buf = synth_sb(17, 0, 96);
        buf[0x0C..0x10].copy_from_slice(&4096u32.to_le_bytes()); // != 1<<17
        assert!(matches!(
            Superblock::parse(&buf),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_non_v4() {
        let mut buf = synth_sb(17, 0, 96);
        buf[0x1C..0x1E].copy_from_slice(&3u16.to_le_bytes());
        assert!(matches!(
            Superblock::parse(&buf),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(matches!(
            Superblock::parse(&[0u8; 16]),
            Err(Error::BadSuperblock(_))
        ));
    }

    fn decoded(b: &[u8]) -> Superblock {
        Superblock::decode(b[..SUPERBLOCK_SIZE].try_into().unwrap())
    }

    /// A superblock `validate_against_device` accepts over a 64 KiB
    /// device: every table in order and inside `bytes_used`.
    fn whole_sb() -> Vec<u8> {
        let mut b = synth_sb(17, 0, 96);
        let put = |b: &mut Vec<u8>, o: usize, v: u64| b[o..o + 8].copy_from_slice(&v.to_le_bytes());
        put(&mut b, 0x28, 0x1_0000); // bytes_used
        put(&mut b, 0x40, 0x100); // inode table
        put(&mut b, 0x48, 0x2000); // directory table
        put(&mut b, 0x50, crate::table::NO_TABLE); // fragments
        put(&mut b, 0x58, crate::table::NO_TABLE); // export
        put(&mut b, 0x30, 0x4000); // id table
        put(&mut b, 0x38, crate::table::NO_TABLE); // xattrs
        b
    }

    #[test]
    fn a_damaged_superblock_still_decodes_every_field() {
        let mut buf = synth_sb(17, 0x1234_0042, 96);
        buf[0..4].copy_from_slice(b"XXXX");
        buf[0x16..0x18].copy_from_slice(&40u16.to_le_bytes());
        let sb = decoded(&buf);
        assert_eq!(sb.magic, u32::from_le_bytes(*b"XXXX"));
        assert_eq!(sb.block_log, 40);
        assert_eq!(sb.root_inode_ref, 0x1234_0042);
        assert_eq!(sb.inode_table_start, 96);
        let faults = sb.header_faults();
        let fields: Vec<_> = faults.iter().map(|f| f.field).collect();
        assert_eq!(fields, ["magic", "block_log"]);
        assert_eq!((faults[1].offset, faults[1].length), (0x16, 2));
    }

    #[test]
    fn the_strict_parse_refuses_with_the_first_fault() {
        let cases: [(usize, &[u8]); 5] = [
            (0x00, b"hsqt"),
            (0x1C, &3u16.to_le_bytes()),
            (0x1E, &1u16.to_le_bytes()),
            (0x16, &11u16.to_le_bytes()),
            (0x0C, &4096u32.to_le_bytes()),
        ];
        for (at, bytes) in cases {
            let mut buf = synth_sb(17, 0, 96);
            buf[at..at + bytes.len()].copy_from_slice(bytes);
            let sb = decoded(&buf);
            let faults = sb.header_faults();
            assert_eq!(faults.len(), 1, "offset {at:#x}: {faults:?}");
            assert_eq!(faults[0].offset, at as u64);
            let strict = Superblock::parse(&buf).unwrap_err().to_string();
            assert_eq!(strict, faults[0].to_error(&sb).to_string());
        }
        assert!(decoded(&synth_sb(17, 0, 96)).header_faults().is_empty());
    }

    #[test]
    fn an_unknown_compressor_is_a_fault_the_parse_leaves_to_the_open() {
        let mut buf = synth_sb(17, 0, 96);
        buf[0x14..0x16].copy_from_slice(&99u16.to_le_bytes());
        let sb = Superblock::parse(&buf).expect("the header parse does not judge the codec");
        let fault = sb.compressor_fault().expect("id 99 names no compressor");
        assert_eq!(
            (fault.field, fault.offset, fault.length),
            ("compression_id", 0x14, 2)
        );
        assert!(matches!(
            fault.to_error(&sb),
            Error::UnsupportedCompression(99)
        ));
    }

    #[test]
    fn every_device_fault_is_listed_and_the_first_is_the_refusal() {
        let sb = decoded(&whole_sb());
        assert!(
            sb.device_faults(0x1_0000).is_empty(),
            "{:?}",
            sb.device_faults(0x1_0000)
        );
        sb.validate_against_device(0x1_0000).unwrap();

        let mut buf = whole_sb();
        buf[0x48..0x50].copy_from_slice(&0x50u64.to_le_bytes()); // directory inside the superblock
        buf[0x30..0x38].copy_from_slice(&0x9_0000u64.to_le_bytes()); // id table past bytes_used
        let sb = decoded(&buf);
        let faults = sb.device_faults(0x1_0000);
        let fields: Vec<_> = faults.iter().map(|f| (f.field, f.rule)).collect();
        assert_eq!(
            fields,
            [
                ("directory_table_start", SuperblockRule::TableBounds),
                ("id_table_start", SuperblockRule::TableBounds),
                ("directory_table_start", SuperblockRule::TableOrder),
                ("root_inode_ref", SuperblockRule::RootInode),
            ]
        );
        let strict = sb
            .validate_against_device(0x1_0000)
            .unwrap_err()
            .to_string();
        assert_eq!(strict, faults[0].to_error(&sb).to_string());
        assert_eq!(sb.faults(0x1_0000), faults, "a sound header adds nothing");
    }

    #[test]
    fn a_device_shorter_than_bytes_used_is_a_fault_on_bytes_used() {
        let sb = decoded(&whole_sb());
        let faults = sb.device_faults(0x8000);
        assert_eq!(faults[0].field, "bytes_used");
        assert_eq!((faults[0].offset, faults[0].length), (0x28, 8));
        let damage = faults[0].damage();
        assert_eq!(damage.kind, crate::damage::DamageKind::Mismatch);
        assert_eq!(damage.structure, "superblock");
        assert!(
            damage.detail.starts_with("bytes_used: "),
            "{}",
            damage.detail
        );
    }
}
