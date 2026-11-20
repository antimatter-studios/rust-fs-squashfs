//! The volume layer: an image as a whole, opened on its superblock alone.
//!
//! [`Filesystem::open`] is the strict open: it refuses an image whose
//! superblock or lookup tables are damaged, which is right for reading
//! files and wrong for finding out what is wrong. A [`Volume`] opens on
//! nothing more than a readable first 96 bytes, so the superblock, the
//! geometry, the health of the image and its raw bytes are all
//! available however damaged the rest is.
//!
//! Read-only by type: a volume holds a [`BlockRead`] and nothing in this
//! crate can write.

use crate::damage::Damage;
use crate::data::{self, BlockContent, DataContext, FileBlock};
use crate::decompress::Compressor;
use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::indirect::{self, IndirectTable, XattrIdTable};
use crate::inode::Inode;
use crate::metatable::{walk_table, TableWalk};
use crate::superblock::{Superblock, SuperblockFault, SUPERBLOCK_SIZE};
use fs_core::BlockRead;
use std::sync::Arc;

/// The device a volume reads: owned, for a volume a caller opens, or
/// borrowed, for the salvage adapter, which is lent a source per call.
enum Device<'d> {
    Owned(Arc<dyn BlockRead>),
    Borrowed(&'d dyn BlockRead),
}

impl<'d> std::ops::Deref for Device<'d> {
    type Target = dyn BlockRead + 'd;
    fn deref(&self) -> &Self::Target {
        match self {
            Device::Owned(dev) => &**dev,
            Device::Borrowed(dev) => *dev,
        }
    }
}

/// An image opened on its superblock, damaged or not.
///
/// `'d` is the device's lifetime: `'static` for a volume opened with
/// [`Volume::open`], which owns its device.
pub struct Volume<'d> {
    dev: Device<'d>,
    sb: Superblock,
    device_size: u64,
}

/// What the volume layer can say about an image as a whole.
#[derive(Debug)]
pub struct VolumeHealth {
    /// Every superblock fault, in the order the strict open meets them.
    pub superblock: Vec<SuperblockFault>,
    /// Why the strict open refuses the image, or `None` when it opens.
    pub refusal: Option<Error>,
}

impl VolumeHealth {
    /// Whether the strict open accepts the image.
    pub fn opens(&self) -> bool {
        self.refusal.is_none()
    }

    /// The superblock faults as damage.
    pub fn damage(&self) -> Vec<Damage> {
        self.superblock
            .iter()
            .map(SuperblockFault::damage)
            .collect()
    }
}

impl Volume<'static> {
    /// Open an image on its superblock.
    ///
    /// Fails only when the first 96 bytes cannot be read: a device shorter
    /// than a superblock, or one that refuses the read. Any value in those
    /// bytes, however wrong, opens; [`Volume::health`] says what is wrong.
    pub fn open(dev: Arc<dyn BlockRead>) -> Result<Self> {
        let device_size = dev.size_bytes();
        if device_size < SUPERBLOCK_SIZE as u64 {
            return Err(Error::BadSuperblock("device shorter than a superblock"));
        }
        let mut bytes = [0u8; SUPERBLOCK_SIZE];
        dev.read_at(0, &mut bytes)?;
        Ok(Volume {
            sb: Superblock::decode(&bytes),
            dev: Device::Owned(dev),
            device_size,
        })
    }
}

impl<'d> Volume<'d> {
    /// A volume over a borrowed device, with a superblock the caller
    /// already holds -- the salvage adapter's view of the source it is
    /// lent, under its own geometry.
    pub(crate) fn borrowed(dev: &'d dyn BlockRead, sb: Superblock) -> Self {
        Volume {
            device_size: dev.size_bytes(),
            dev: Device::Borrowed(dev),
            sb,
        }
    }

    /// The device, lent read-only.
    pub fn device(&self) -> &(dyn BlockRead + 'd) {
        &*self.dev
    }

    /// The superblock as the bytes hold it, unjudged.
    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// The size of the device the image is on, in bytes.
    pub fn device_size(&self) -> u64 {
        self.device_size
    }

    /// The image's compressor, or `None` when the id names none.
    pub fn compressor(&self) -> Option<Compressor> {
        self.sb.compressor().ok()
    }

    /// Every superblock fault, in the order the strict open meets them.
    pub fn superblock_faults(&self) -> Vec<SuperblockFault> {
        self.sb.faults(self.device_size)
    }

    /// The health of the image as a whole: its superblock faults, and
    /// whether the strict open accepts it.
    ///
    /// The strict open is attempted to answer the second part, so this
    /// reads the lookup tables as well as the superblock.
    pub fn health(&self) -> VolumeHealth {
        VolumeHealth {
            superblock: self.superblock_faults(),
            refusal: self.filesystem().err(),
        }
    }

    /// The strict open of the same device: the file layer, for an image
    /// that is healthy enough to read files from.
    ///
    /// Only a volume that owns its device has one: the strict reader keeps
    /// the device for the life of the mount.
    pub fn filesystem(&self) -> Result<Filesystem> {
        match &self.dev {
            Device::Owned(dev) => Filesystem::open(dev.clone()),
            Device::Borrowed(_) => Err(Error::BadSuperblock(
                "a volume over a borrowed device opens for salvage only",
            )),
        }
    }

    /// The inode table, framed block by block with every damaged block
    /// reported: `[inode_table_start, directory_table_start)`.
    ///
    /// Decodes every block in the table, so it costs a pass over all the
    /// inode metadata. The bounds are the superblock's as they stand; a
    /// damaged one makes a walk of the wrong bytes, which the walk then
    /// reports as damage rather than as inodes.
    pub fn inode_table(&self) -> TableWalk {
        walk_table(
            &*self.dev,
            self.compressor(),
            self.sb.inode_table_start,
            self.sb.directory_table_start,
            "inode table",
        )
    }

    /// The directory table, framed block by block with every damaged block
    /// reported: from `directory_table_start` to where the first table
    /// after it begins.
    pub fn directory_table(&self) -> TableWalk {
        walk_table(
            &*self.dev,
            self.compressor(),
            self.sb.directory_table_start,
            crate::fs::directory_table_end(&*self.dev, &self.sb),
            "directory table",
        )
    }

    /// The uid/gid id table, read block by block.
    pub fn id_table(&self) -> IndirectTable {
        indirect::id_table(&*self.dev, &self.sb)
    }

    /// The fragment table, read block by block.
    pub fn fragment_table(&self) -> IndirectTable {
        indirect::fragment_table(&*self.dev, &self.sb)
    }

    /// The export table, read block by block, or `None` when the image
    /// has none.
    pub fn export_table(&self) -> Option<IndirectTable> {
        indirect::export_table(&*self.dev, &self.sb)
    }

    /// The xattr id table, read block by block, or `None` when the image
    /// has no attributes.
    pub fn xattr_id_table(&self) -> Option<XattrIdTable> {
        indirect::xattr_id_table(&*self.dev, &self.sb)
    }

    /// A regular file's blocks, laid out from its inode alone.
    pub fn file_blocks(&self, inode: &Inode) -> Vec<FileBlock> {
        data::file_map(inode, self.sb.block_size)
    }

    /// Read one block of a file, with `fragments` for its tail, and say
    /// what it holds: its bytes, zeros the format records, or why its
    /// bytes are unknown.
    pub fn read_file_block(&self, fragments: &IndirectTable, block: &FileBlock) -> BlockContent {
        let lookup = |i: u32| indirect::fragment_entry(fragments, u64::from(i));
        data::read_file_block(
            &DataContext {
                dev: &*self.dev,
                comp: self.compressor(),
                block_size: self.sb.block_size,
                bytes_used: self.sb.bytes_used.min(self.device_size),
                fragment: &lookup,
            },
            block,
        )
    }

    /// Read source bytes as they are stored, needing no valid structure.
    ///
    /// Reads `buf.len()` bytes from `offset`, or fewer when the device
    /// ends first; returns how many, and 0 at or past the end. A device
    /// that refuses the read is an error, never a short count.
    pub fn read_raw(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= self.device_size {
            return Ok(0);
        }
        let room = self.device_size - offset;
        let n = buf.len().min(usize::try_from(room).unwrap_or(usize::MAX));
        self.dev.read_at(offset, &mut buf[..n])?;
        Ok(n)
    }
}
