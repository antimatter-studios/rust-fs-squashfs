//! SquashFS superblock parsing.
//!
//! The superblock lives at byte offset 0 and is 96 bytes long. Field
//! layout matches `struct squashfs_super_block` in squashfs-tools'
//! `squashfs_fs.h`; all fields are little-endian. Names mirror the C
//! struct with the redundant prefixes dropped.

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

impl Superblock {
    /// Parse + validate a superblock from a 96-byte buffer.
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() < SUPERBLOCK_SIZE {
            return Err(Error::BadSuperblock("buffer shorter than 96 bytes"));
        }
        let rd_u16 = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
        let rd_u32 = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let rd_u64 = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());

        let magic = rd_u32(0x00);
        if magic != SQUASHFS_MAGIC {
            return Err(Error::NotSquashfs);
        }

        // The version first: a 3.x superblock shares the magic and the
        // major's offset, but not `block_log`'s, and was refused as a bad
        // block size rather than as the version it is.
        let version_major = rd_u16(0x1C);
        let version_minor = rd_u16(0x1E);
        if version_major != 4 {
            return Err(Error::BadSuperblock("only SquashFS 4.x is supported"));
        }
        // As the kernel does (`s_minor > SQUASHFS_MINOR`): a later minor
        // may change the layout this parser assumes.
        if version_minor > SQUASHFS_MINOR {
            return Err(Error::BadSuperblock(
                "unknown SquashFS minor version (only 4.0 is supported)",
            ));
        }

        let block_log = rd_u16(0x16);
        if !(MIN_BLOCK_LOG..=MAX_BLOCK_LOG).contains(&block_log) {
            return Err(Error::BadSuperblock("block_log out of range"));
        }
        let block_size = rd_u32(0x0C);
        if block_size != 1u32 << block_log {
            return Err(Error::BadSuperblock("block_size != 1 << block_log"));
        }

        Ok(Superblock {
            magic,
            inode_count: rd_u32(0x04),
            modification_time: rd_u32(0x08),
            block_size,
            fragment_entry_count: rd_u32(0x10),
            compression_id: rd_u16(0x14),
            block_log,
            flags: rd_u16(0x18),
            id_count: rd_u16(0x1A),
            version_major,
            version_minor,
            root_inode_ref: rd_u64(0x20),
            bytes_used: rd_u64(0x28),
            id_table_start: rd_u64(0x30),
            xattr_id_table_start: rd_u64(0x38),
            inode_table_start: rd_u64(0x40),
            directory_table_start: rd_u64(0x48),
            fragment_table_start: rd_u64(0x50),
            export_table_start: rd_u64(0x58),
        })
    }

    /// Check the superblock's sizes and table starts against the device
    /// it was read from.
    ///
    /// `parse` sees 96 bytes and nothing else, so everything here was
    /// taken on trust: a header claiming `bytes_used` = 1 TiB over a
    /// 20 KiB file mounted and published a terabyte through
    /// `fs_squashfs_get_volume_info`, and table starts past the end, inside
    /// the superblock or out of order opened cleanly and failed later as a
    /// short read at an absurd offset, if at all (#48).
    ///
    /// The layout `mksquashfs` writes, and the kernel's
    /// `squashfs_fill_super` relies on, is the inode table, then the
    /// directory table, then the fragment, export, id and xattr tables'
    /// index arrays in that order, each preceded by its own metadata, all
    /// below `bytes_used`. Absent optional tables carry [`NO_TABLE`] and
    /// are left out of the ordering.
    ///
    /// [`NO_TABLE`]: crate::table::NO_TABLE
    pub fn validate_against_device(&self, device_size: u64) -> Result<()> {
        use crate::table::NO_TABLE;
        if self.bytes_used > device_size {
            return Err(Error::BadSuperblock(
                "bytes_used exceeds the size of the device",
            ));
        }
        let start = SUPERBLOCK_SIZE as u64;
        let in_image = |t: u64| (start..self.bytes_used).contains(&t);
        // Required tables.
        for t in [
            self.inode_table_start,
            self.directory_table_start,
            self.id_table_start,
        ] {
            if !in_image(t) {
                return Err(Error::BadSuperblock(
                    "a table starts outside [superblock end, bytes_used)",
                ));
            }
        }
        // Optional tables: absent, or inside the image.
        for t in [
            self.fragment_table_start,
            self.export_table_start,
            self.xattr_id_table_start,
        ] {
            if t != NO_TABLE && !in_image(t) {
                return Err(Error::BadSuperblock(
                    "a table starts outside [superblock end, bytes_used)",
                ));
            }
        }
        // The inode table is strictly before the directory table (the
        // root inode lives in it); every later table starts at or after
        // the one before it.
        if self.inode_table_start >= self.directory_table_start {
            return Err(Error::BadSuperblock(
                "table starts are out of order (inode table not before directory table)",
            ));
        }
        let mut prev = self.directory_table_start;
        for t in [
            self.fragment_table_start,
            self.export_table_start,
            self.id_table_start,
            self.xattr_id_table_start,
        ] {
            if t == NO_TABLE {
                continue;
            }
            if t < prev {
                return Err(Error::BadSuperblock("table starts are out of order"));
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
            return Err(Error::BadSuperblock(
                "root inode reference points outside the inode table",
            ));
        }
        Ok(())
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
}
