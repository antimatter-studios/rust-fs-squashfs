//! SquashFS directory-listing parser.
//!
//! A directory's listing (read from the directory table) is a sequence of
//! *directory headers*, each followed by its entries:
//!
//! Header (12 bytes): `u32 count` (entries that follow, minus one),
//! `u32 start` (the inode-table metadata-block offset shared by these
//! entries — the high part of each child's inode reference), `u32
//! inode_number` (base; each entry stores a signed delta).
//!
//! Entry (8 bytes + name): `u16 offset` (low 16 bits of the inode
//! reference), `i16 inode_offset` (delta to the header's base inode
//! number), `u16 type` (basic inode type 1-7), `u16 name_size` (name
//! length minus one), then `name_size + 1` bytes of name.
//!
//! This module parses a fully-materialised listing buffer; assembling that
//! buffer from the directory table is [`crate::fs`]'s job.

use crate::error::{Error, Result};

/// SquashFS caps names at 256 bytes.
///
/// `pub` because the C ABI's name buffer is sized from it: the two
/// were separate `256`s, and one of them had to leave room for a NUL
/// (#56).
pub const SQUASHFS_NAME_LEN: usize = 256;

/// A directory header describes at most this many entries.
///
/// The same number as [`SQUASHFS_NAME_LEN`] and an entirely unrelated
/// quantity: that one is a byte length, this is a count. They were both
/// written as a bare `256` in this file, which is how a reader ends up
/// assuming one bound explains the other.
const MAX_ENTRIES_PER_HEADER: usize = 256;

/// One resolved directory entry.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: Vec<u8>,
    /// Metadata reference to the child inode (feed to [`crate::Inode::read`]).
    pub inode_ref: u64,
    /// Basic inode type id (1-7) recorded in the entry.
    pub type_id: u16,
    /// Child inode number (header base + signed per-entry delta).
    pub inode_number: u32,
}

/// A directory listing decoded as far as it goes.
#[derive(Debug, Clone)]
pub struct Listing {
    /// Every entry before the first fault, in listing order.
    pub entries: Vec<DirEntry>,
    /// Where the listing stopped making sense, and why; `None` when the
    /// whole buffer decoded.
    pub fault: Option<ListingFault>,
}

/// Where a listing stopped decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListingFault {
    /// Byte offset into the listing of the header or entry that is wrong.
    pub offset: usize,
    /// Why, in the strict parse's words.
    pub reason: &'static str,
}

/// Parse a directory listing buffer into entries (linear order). `.` and
/// `..` are implicit in SquashFS and are NOT included.
///
/// [`parse_listing_partial`], refused on its fault.
pub fn parse_listing(buf: &[u8]) -> Result<Vec<DirEntry>> {
    let listing = parse_listing_partial(buf);
    match listing.fault {
        Some(fault) => Err(Error::BadDirent(fault.reason)),
        None => Ok(listing.entries),
    }
}

/// Parse a directory listing, keeping every entry before the first
/// fault rather than refusing the whole listing for it.
///
/// A listing is a run of headers, each over up to 256 entries, so a
/// fault in one header or entry leaves nothing after it to frame by: the
/// parse stops there and reports where.
pub fn parse_listing_partial(buf: &[u8]) -> Listing {
    let mut out = Vec::new();
    let mut p = 0usize;
    let mut at: usize;
    macro_rules! fault {
        ($reason:expr) => {
            return Listing {
                entries: out,
                fault: Some(ListingFault {
                    offset: at,
                    reason: $reason,
                }),
            }
        };
    }

    while p < buf.len() {
        at = p;
        // A trailing run shorter than a header is malformed; but an exact
        // end is fine (handled by the while condition).
        if p + 12 > buf.len() {
            fault!("truncated directory header");
        }
        let count = u32::from_le_bytes(buf[p..p + 4].try_into().unwrap());
        let start = u32::from_le_bytes(buf[p + 4..p + 8].try_into().unwrap());
        let base_inode = u32::from_le_bytes(buf[p + 8..p + 12].try_into().unwrap());
        p += 12;

        // `count` is entries-minus-one.
        let Some(entries) = (count as usize).checked_add(1) else {
            fault!("directory header count overflow");
        };
        if entries > MAX_ENTRIES_PER_HEADER {
            fault!("directory header count > 256");
        }

        for _ in 0..entries {
            at = p;
            if p + 8 > buf.len() {
                fault!("truncated directory entry header");
            }
            let offset = u16::from_le_bytes(buf[p..p + 2].try_into().unwrap());
            let inode_offset = i16::from_le_bytes(buf[p + 2..p + 4].try_into().unwrap());
            let type_id = u16::from_le_bytes(buf[p + 4..p + 6].try_into().unwrap());
            let name_size = u16::from_le_bytes(buf[p + 6..p + 8].try_into().unwrap());
            p += 8;

            let name_len = name_size as usize + 1;
            if name_len > SQUASHFS_NAME_LEN {
                fault!("entry name too long");
            }
            if p + name_len > buf.len() {
                fault!("truncated directory entry name");
            }
            let name = buf[p..p + name_len].to_vec();
            p += name_len;

            if name == b"." || name == b".." {
                fault!("directory entry name is reserved");
            }
            if name.contains(&b'/') {
                fault!("directory entry name contains '/'");
            }
            if name.contains(&0) {
                fault!("directory entry name contains NUL");
            }

            let inode_ref = ((start as u64) << 16) | (offset as u64);
            // The per-entry delta is a signed i16 added to the header base.
            let inode_number = (base_inode as i64 + inode_offset as i64) as u32;

            out.push(DirEntry {
                name,
                inode_ref,
                type_id,
                inode_number,
            });
        }
    }
    Listing {
        entries: out,
        fault: None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a one-header listing with the given (name, type, offset,
    /// inode_delta) entries sharing metadata-block `start`/`base_inode`.
    pub(crate) fn synth_listing(
        start: u32,
        base_inode: u32,
        entries: &[(&[u8], u16, u16, i16)],
    ) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&((entries.len() as u32) - 1).to_le_bytes());
        b.extend_from_slice(&start.to_le_bytes());
        b.extend_from_slice(&base_inode.to_le_bytes());
        for (name, type_id, offset, delta) in entries {
            b.extend_from_slice(&offset.to_le_bytes());
            b.extend_from_slice(&delta.to_le_bytes());
            b.extend_from_slice(&type_id.to_le_bytes());
            b.extend_from_slice(&((name.len() as u16) - 1).to_le_bytes());
            b.extend_from_slice(name);
        }
        b
    }

    #[test]
    fn parses_single_header() {
        let buf = synth_listing(5, 100, &[(b"hello.txt", 2, 0x40, 0), (b"sub", 1, 0x80, 3)]);
        let entries = parse_listing(&buf).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, b"hello.txt");
        assert_eq!(entries[0].type_id, 2);
        assert_eq!(entries[0].inode_ref, (5u64 << 16) | 0x40);
        assert_eq!(entries[0].inode_number, 100);
        assert_eq!(entries[1].name, b"sub");
        assert_eq!(entries[1].inode_ref, (5u64 << 16) | 0x80);
        assert_eq!(entries[1].inode_number, 103);
    }

    #[test]
    fn a_partial_parse_keeps_every_entry_before_the_fault() {
        let mut buf = synth_listing(5, 100, &[(b"one", 2, 0x10, 0), (b"two", 2, 0x20, 1)]);
        let second = buf.len();
        buf.extend_from_slice(&synth_listing(
            6,
            200,
            &[(b"ok", 2, 0x30, 0), (b"b/d", 2, 0x40, 1)],
        ));
        let bad_entry = buf.len() - (8 + 3);
        let listing = parse_listing_partial(&buf);
        let names: Vec<_> = listing.entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, [b"one".to_vec(), b"two".to_vec(), b"ok".to_vec()]);
        assert_eq!(
            listing.fault,
            Some(ListingFault {
                offset: bad_entry,
                reason: "directory entry name contains '/'"
            })
        );
        assert!(second < bad_entry);
        // The strict parse refuses the same buffer with the same reason.
        match parse_listing(&buf) {
            Err(Error::BadDirent(why)) => assert_eq!(why, "directory entry name contains '/'"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_truncated_header_is_a_fault_at_its_start() {
        let mut buf = synth_listing(5, 100, &[(b"one", 2, 0x10, 0)]);
        let cut = buf.len();
        buf.extend_from_slice(&[1, 2, 3]);
        let listing = parse_listing_partial(&buf);
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.fault.unwrap().offset, cut);
    }

    #[test]
    fn empty_buffer_yields_no_entries() {
        assert!(parse_listing(&[]).unwrap().is_empty());
    }

    #[test]
    fn negative_inode_delta() {
        let buf = synth_listing(0, 100, &[(b"a", 2, 0, -10)]);
        let e = parse_listing(&buf).unwrap();
        assert_eq!(e[0].inode_number, 90);
    }

    #[test]
    fn rejects_truncated_name() {
        let mut buf = synth_listing(0, 1, &[(b"abcdef", 2, 0, 0)]);
        buf.truncate(buf.len() - 2); // chop part of the name
        assert!(matches!(parse_listing(&buf), Err(Error::BadDirent(_))));
    }

    #[test]
    fn rejects_truncated_header() {
        assert!(matches!(parse_listing(&[0u8; 5]), Err(Error::BadDirent(_))));
    }

    #[test]
    fn rejects_reserved_directory_entry_names() {
        for name in [b".".as_slice(), b"..".as_slice()] {
            let buf = synth_listing(0, 1, &[(name, 2, 0, 0)]);
            assert!(matches!(
                parse_listing(&buf),
                Err(Error::BadDirent("directory entry name is reserved"))
            ));
        }
    }

    #[test]
    fn rejects_directory_entry_names_with_slash() {
        let buf = synth_listing(0, 1, &[(b"nested/name", 2, 0, 0)]);
        assert!(matches!(
            parse_listing(&buf),
            Err(Error::BadDirent("directory entry name contains '/'"))
        ));
    }

    #[test]
    fn rejects_directory_entry_names_with_nul() {
        let buf = synth_listing(0, 1, &[(b"bad\0name", 2, 0, 0)]);
        assert!(matches!(
            parse_listing(&buf),
            Err(Error::BadDirent("directory entry name contains NUL"))
        ));
    }
}
