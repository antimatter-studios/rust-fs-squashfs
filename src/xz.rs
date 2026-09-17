//! `.xz` streams whose filter chain puts a branch-call-jump converter in
//! front of LZMA2 (#52).
//!
//! `mksquashfs -comp xz -Xbcj <arch>` filters every data block through a
//! BCJ converter whenever that makes the block smaller, and keeps the
//! plain stream when it does not. `lzma_rs::xz_decompress` implements
//! LZMA2 alone and refuses any other chain, so on such an image -- the
//! OpenWrt default, and common in firmware -- every file failed to read
//! while the directory tree listed perfectly, because metadata blocks are
//! never filtered.
//!
//! This parses the container itself, decodes the raw LZMA2 payload with
//! `lzma_rs::lzma2_decompress`, verifies the block's check, and runs the
//! converter backwards. The converters are the xz-embedded ones (public
//! domain), in decode direction, over a whole block at once: each
//! SquashFS block is its own stream, so the converter's position starts
//! at the filter's `start_offset` (zero unless its properties say
//! otherwise). x86, PowerPC, IA-64, ARM, ARM-Thumb and SPARC are
//! implemented; ARM64 and RISC-V are refused by name.

use std::io::Cursor;

use crate::error::{Error, Result};

const STREAM_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0x00];
const FILTER_LZMA2: u64 = 0x21;

/// The BCJ converters, by xz filter id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bcj {
    X86,
    PowerPc,
    Ia64,
    Arm,
    ArmThumb,
    Sparc,
}

impl Bcj {
    fn from_id(id: u64) -> Result<Self> {
        Ok(match id {
            0x04 => Bcj::X86,
            0x05 => Bcj::PowerPc,
            0x06 => Bcj::Ia64,
            0x07 => Bcj::Arm,
            0x08 => Bcj::ArmThumb,
            0x09 => Bcj::Sparc,
            0x0A => {
                return Err(Error::BadMetadata(
                    "xz: the ARM64 BCJ filter is not implemented",
                ))
            }
            0x0B => {
                return Err(Error::BadMetadata(
                    "xz: the RISC-V BCJ filter is not implemented",
                ))
            }
            0x03 => {
                return Err(Error::BadMetadata(
                    "xz: the delta filter is not implemented",
                ))
            }
            _ => {
                return Err(Error::BadMetadata(
                    "xz: unknown filter in the block's chain",
                ))
            }
        })
    }
}

/// Whether `stream`'s first block names a filter chain other than plain
/// LZMA2, which `lzma_rs::xz_decompress` cannot decode. `false` for
/// anything that does not parse, so the caller's existing path reports it.
pub(crate) fn has_filters(stream: &[u8]) -> bool {
    let Some(header) = stream.get(12..) else {
        return false;
    };
    if stream[..6] != STREAM_MAGIC {
        return false;
    }
    match header.first() {
        Some(&size) if size != 0 => header.get(1).is_some_and(|flags| flags & 0x03 != 0),
        _ => false,
    }
}

/// Decode a whole `.xz` stream whose blocks may carry one BCJ filter
/// before LZMA2, producing at most `max_out` bytes.
pub(crate) fn decompress(stream: &[u8], max_out: usize) -> Result<Vec<u8>> {
    decompress_with(stream, max_out, true)
}

/// [`decompress`], optionally without running the converter -- which is
/// how the tests show a fixture's converter really changed bytes. The
/// check is skipped with it, since it covers the converted bytes.
fn decompress_with(stream: &[u8], max_out: usize, run_filter: bool) -> Result<Vec<u8>> {
    let bad = Error::BadMetadata;
    if stream.len() < 12 || stream[..6] != STREAM_MAGIC {
        return Err(bad("xz: not an xz stream"));
    }
    let flags = &stream[6..8];
    if crc32(flags) != u32::from_le_bytes(stream[8..12].try_into().unwrap()) || flags[0] != 0 {
        return Err(bad("xz: stream header is damaged"));
    }
    let check = flags[1] & 0x0F;
    let check_len = match check {
        0x00 => 0,
        0x01 => 4,
        0x04 => 8,
        0x0A => 32,
        _ => return Err(bad("xz: unknown check type")),
    };

    let mut out = Vec::new();
    let mut pos = 12usize;
    loop {
        let size_byte = *stream
            .get(pos)
            .ok_or(bad("xz: stream ends before its index"))?;
        if size_byte == 0 {
            // The index: every block has been read.
            return Ok(out);
        }
        let block_start = pos;
        let header_len = (usize::from(size_byte) + 1) * 4;
        let header = stream
            .get(pos..pos + header_len)
            .ok_or(bad("xz: block header runs off the stream"))?;
        let stored = u32::from_le_bytes(header[header_len - 4..].try_into().unwrap());
        if crc32(&header[..header_len - 4]) != stored {
            return Err(bad("xz: block header checksum mismatch"));
        }
        let block_flags = header[1];
        if block_flags & 0x3C != 0 {
            return Err(bad("xz: reserved block flags set"));
        }
        let mut at = 2usize;
        let body = &header[..header_len - 4];
        let compressed_size = if block_flags & 0x40 != 0 {
            Some(varint(body, &mut at)?)
        } else {
            None
        };
        let uncompressed_size = if block_flags & 0x80 != 0 {
            Some(varint(body, &mut at)?)
        } else {
            None
        };
        let filters = usize::from(block_flags & 0x03) + 1;
        let mut bcj: Option<(Bcj, u32)> = None;
        for index in 0..filters {
            let id = varint(body, &mut at)?;
            let props_len = varint(body, &mut at)? as usize;
            let props = body
                .get(at..at + props_len)
                .ok_or(bad("xz: filter properties run off the block header"))?;
            at += props_len;
            let last = index + 1 == filters;
            match (last, id) {
                (true, FILTER_LZMA2) => {}
                (true, _) => return Err(bad("xz: the chain does not end in LZMA2")),
                (false, FILTER_LZMA2) => return Err(bad("xz: LZMA2 before the end of the chain")),
                (false, _) if bcj.is_some() => {
                    return Err(bad("xz: more than one filter before LZMA2"))
                }
                (false, id) => {
                    let start = match props {
                        [] => 0,
                        [a, b, c, d] => u32::from_le_bytes([*a, *b, *c, *d]),
                        _ => return Err(bad("xz: BCJ properties are neither empty nor 4 bytes")),
                    };
                    bcj = Some((Bcj::from_id(id)?, start));
                }
            }
        }
        if body[at..].iter().any(|&b| b != 0) {
            return Err(bad("xz: block header padding is not zero"));
        }

        let data_start = pos + header_len;
        let payload = stream
            .get(data_start..)
            .ok_or(bad("xz: block has no payload"))?;
        let payload = match compressed_size {
            Some(n) => payload
                .get(..usize::try_from(n).map_err(|_| bad("xz: compressed size too large"))?)
                .ok_or(bad("xz: compressed size runs off the stream"))?,
            None => payload,
        };
        let mut cursor = Cursor::new(payload);
        let mut block = Capped {
            buf: Vec::new(),
            limit: max_out.saturating_sub(out.len()).saturating_add(1),
        };
        lzma_rs::lzma2_decompress(&mut cursor, &mut block)
            .map_err(|_| bad("xz decompression failed"))?;
        let consumed = cursor.position() as usize;
        if compressed_size.is_some_and(|n| n as usize != consumed) {
            return Err(bad("xz: compressed size disagrees with the payload"));
        }
        let mut block = block.buf;
        if uncompressed_size.is_some_and(|n| n != block.len() as u64) {
            return Err(bad("xz: uncompressed size disagrees with the payload"));
        }
        if out.len() + block.len() > max_out {
            return Err(bad("decompressed block exceeds max size"));
        }

        // Padding to a multiple of four from the block's start, then the
        // check over the uncompressed (filter-decoded) bytes.
        let mut end = data_start + consumed;
        while !(end - block_start).is_multiple_of(4) {
            if stream.get(end) != Some(&0) {
                return Err(bad("xz: block padding is not zero"));
            }
            end += 1;
        }
        let stored_check = stream
            .get(end..end + check_len)
            .ok_or(bad("xz: block check runs off the stream"))?;
        if let Some((kind, start)) = bcj {
            if !run_filter {
                out.extend_from_slice(&block);
                pos = end + check_len;
                continue;
            }
            decode(kind, start, &mut block);
        }
        match check {
            0x01 if crc32(&block).to_le_bytes() != stored_check => {
                return Err(bad("xz: block CRC32 mismatch"));
            }
            0x04 if crc64(&block).to_le_bytes() != stored_check => {
                return Err(bad("xz: block CRC64 mismatch"));
            }
            0x0A => return Err(bad("xz: SHA-256 checked blocks are not supported")),
            _ => {}
        }
        out.extend_from_slice(&block);
        pos = end + check_len;
    }
}

/// A sink that refuses to grow past `limit`, as `decompress.rs`'s own.
struct Capped {
    buf: Vec<u8>,
    limit: usize,
}

impl std::io::Write for Capped {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() + data.len() > self.limit {
            return Err(std::io::Error::other(
                "decompressed block exceeds the block size",
            ));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn varint(buf: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for i in 0..9 {
        let b = *buf
            .get(*at)
            .ok_or(Error::BadMetadata("xz: varint runs off the block header"))?;
        *at += 1;
        value |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            if i > 0 && b == 0 {
                return Err(Error::BadMetadata("xz: varint is not minimally encoded"));
            }
            return Ok(value);
        }
    }
    Err(Error::BadMetadata("xz: varint longer than nine bytes"))
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    crc.sum()
}

/// CRC-64/XZ: ECMA-182, reflected.
fn crc64(data: &[u8]) -> u64 {
    const POLY: u64 = 0xC96C_5795_D787_0F42;
    let mut crc = !0u64;
    for &b in data {
        crc ^= u64::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Undo `kind`'s branch conversion over a whole block that began at
/// stream position `start`.
pub(crate) fn decode(kind: Bcj, start: u32, buf: &mut [u8]) {
    match kind {
        Bcj::X86 => x86(start, buf),
        Bcj::PowerPc => powerpc(start, buf),
        Bcj::Ia64 => ia64(start, buf),
        Bcj::Arm => arm(start, buf),
        Bcj::ArmThumb => armthumb(start, buf),
        Bcj::Sparc => sparc(start, buf),
    }
}

fn x86(start: u32, buf: &mut [u8]) {
    const ALLOWED: [bool; 8] = [true, true, true, false, true, false, false, false];
    const BIT_NUM: [u32; 8] = [0, 1, 2, 2, 3, 3, 3, 3];
    let msbyte = |b: u8| b == 0x00 || b == 0xFF;
    if buf.len() <= 4 {
        return;
    }
    let size = buf.len() - 4;
    let mut prev_pos = usize::MAX;
    let mut prev_mask = 0u32;
    let mut i = 0usize;
    while i < size {
        if buf[i] & 0xFE != 0xE8 {
            i += 1;
            continue;
        }
        let gap = i.wrapping_sub(prev_pos);
        if gap > 3 {
            prev_mask = 0;
        } else {
            prev_mask = (prev_mask << (gap - 1)) & 7;
            if prev_mask != 0 {
                let b = buf[i + 4 - BIT_NUM[prev_mask as usize] as usize];
                if !ALLOWED[prev_mask as usize] || msbyte(b) {
                    prev_pos = i;
                    prev_mask = (prev_mask << 1) | 1;
                    i += 1;
                    continue;
                }
            }
        }
        prev_pos = i;
        if msbyte(buf[i + 4]) {
            let mut src = u32::from_le_bytes(buf[i + 1..i + 5].try_into().unwrap());
            let mut dest;
            loop {
                dest = src.wrapping_sub(start.wrapping_add(i as u32).wrapping_add(5));
                if prev_mask == 0 {
                    break;
                }
                let j = BIT_NUM[prev_mask as usize] * 8;
                let b = (dest >> (24 - j)) as u8;
                if !msbyte(b) {
                    break;
                }
                src = dest ^ ((1u32 << (32 - j)) - 1);
            }
            dest &= 0x01FF_FFFF;
            dest |= 0u32.wrapping_sub(dest & 0x0100_0000);
            buf[i + 1..i + 5].copy_from_slice(&dest.to_le_bytes());
            i += 5;
        } else {
            prev_mask = (prev_mask << 1) | 1;
            i += 1;
        }
    }
}

fn powerpc(start: u32, buf: &mut [u8]) {
    let size = buf.len() & !3;
    for i in (0..size).step_by(4) {
        let mut instr = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap());
        if instr & 0xFC00_0003 == 0x4800_0001 {
            instr &= 0x03FF_FFFC;
            instr = instr.wrapping_sub(start.wrapping_add(i as u32));
            instr &= 0x03FF_FFFC;
            instr |= 0x4800_0001;
            buf[i..i + 4].copy_from_slice(&instr.to_be_bytes());
        }
    }
}

fn ia64(start: u32, buf: &mut [u8]) {
    const BRANCH_TABLE: [u32; 32] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 6, 6, 0, 0, 7, 7, 4, 4, 0, 0, 4, 4,
        0, 0,
    ];
    let size = buf.len() & !15;
    for i in (0..size).step_by(16) {
        let mask = BRANCH_TABLE[usize::from(buf[i] & 0x1F)];
        let mut bit_pos = 5u32;
        for slot in 0..3 {
            if (mask >> slot) & 1 != 0 {
                let byte_pos = (bit_pos >> 3) as usize;
                let bit_res = bit_pos & 7;
                let mut instr = 0u64;
                for j in 0..6 {
                    instr |= u64::from(buf[i + j + byte_pos]) << (8 * j);
                }
                let mut norm = instr >> bit_res;
                if (norm >> 37) & 0x0F == 0x05 && (norm >> 9) & 0x07 == 0 {
                    let mut addr = ((norm >> 13) & 0x0F_FFFF) as u32;
                    addr |= (((norm >> 36) & 1) as u32) << 20;
                    addr <<= 4;
                    addr = addr.wrapping_sub(start.wrapping_add(i as u32));
                    addr >>= 4;
                    norm &= !(0x8F_FFFFu64 << 13);
                    norm |= u64::from(addr & 0x0F_FFFF) << 13;
                    norm |= u64::from(addr & 0x10_0000) << (36 - 20);
                    instr &= (1u64 << bit_res) - 1;
                    instr |= norm << bit_res;
                    for j in 0..6 {
                        buf[i + j + byte_pos] = (instr >> (8 * j)) as u8;
                    }
                }
            }
            bit_pos += 41;
        }
    }
}

fn arm(start: u32, buf: &mut [u8]) {
    let size = buf.len() & !3;
    for i in (0..size).step_by(4) {
        if buf[i + 3] == 0xEB {
            let mut addr =
                u32::from(buf[i]) | (u32::from(buf[i + 1]) << 8) | (u32::from(buf[i + 2]) << 16);
            addr <<= 2;
            addr = addr.wrapping_sub(start.wrapping_add(i as u32).wrapping_add(8));
            addr >>= 2;
            buf[i] = addr as u8;
            buf[i + 1] = (addr >> 8) as u8;
            buf[i + 2] = (addr >> 16) as u8;
        }
    }
}

fn armthumb(start: u32, buf: &mut [u8]) {
    if buf.len() < 4 {
        return;
    }
    let size = buf.len() - 4;
    let mut i = 0usize;
    while i <= size {
        if buf[i + 1] & 0xF8 == 0xF0 && buf[i + 3] & 0xF8 == 0xF8 {
            let mut addr = ((u32::from(buf[i + 1]) & 0x07) << 19)
                | (u32::from(buf[i]) << 11)
                | ((u32::from(buf[i + 3]) & 0x07) << 8)
                | u32::from(buf[i + 2]);
            addr <<= 1;
            addr = addr.wrapping_sub(start.wrapping_add(i as u32).wrapping_add(4));
            addr >>= 1;
            buf[i + 1] = 0xF0 | ((addr >> 19) & 0x07) as u8;
            buf[i] = (addr >> 11) as u8;
            buf[i + 3] = 0xF8 | ((addr >> 8) & 0x07) as u8;
            buf[i + 2] = addr as u8;
            i += 2;
        }
        i += 2;
    }
}

fn sparc(start: u32, buf: &mut [u8]) {
    let size = buf.len() & !3;
    for i in (0..size).step_by(4) {
        let mut instr = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap());
        if instr >> 22 == 0x100 || instr >> 22 == 0x1FF {
            instr <<= 2;
            instr = instr.wrapping_sub(start.wrapping_add(i as u32));
            instr >>= 2;
            instr =
                0x4000_0000u32.wrapping_sub(instr & 0x40_0000) | 0x4000_0000 | (instr & 0x3F_FFFF);
            buf[i..i + 4].copy_from_slice(&instr.to_be_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Streams made by liblzma (Python's `lzma`) over bytes shaped for each
    /// converter, beside the bytes they were made from.
    const FIXTURES: [(&str, &[u8], &[u8]); 8] = [
        (
            "x86",
            include_bytes!("../tests/fixtures/xz-bcj/x86.xz"),
            include_bytes!("../tests/fixtures/xz-bcj/x86.bin"),
        ),
        (
            "x86, CRC64",
            include_bytes!("../tests/fixtures/xz-bcj/x86-crc64.xz"),
            include_bytes!("../tests/fixtures/xz-bcj/x86.bin"),
        ),
        (
            "x86, start offset 4096",
            include_bytes!("../tests/fixtures/xz-bcj/x86-start.xz"),
            include_bytes!("../tests/fixtures/xz-bcj/x86-start.bin"),
        ),
        (
            "powerpc",
            include_bytes!("../tests/fixtures/xz-bcj/powerpc.xz"),
            include_bytes!("../tests/fixtures/xz-bcj/powerpc.bin"),
        ),
        (
            "ia64",
            include_bytes!("../tests/fixtures/xz-bcj/ia64.xz"),
            include_bytes!("../tests/fixtures/xz-bcj/ia64.bin"),
        ),
        (
            "arm",
            include_bytes!("../tests/fixtures/xz-bcj/arm.xz"),
            include_bytes!("../tests/fixtures/xz-bcj/arm.bin"),
        ),
        (
            "armthumb",
            include_bytes!("../tests/fixtures/xz-bcj/armthumb.xz"),
            include_bytes!("../tests/fixtures/xz-bcj/armthumb.bin"),
        ),
        (
            "sparc",
            include_bytes!("../tests/fixtures/xz-bcj/sparc.xz"),
            include_bytes!("../tests/fixtures/xz-bcj/sparc.bin"),
        ),
    ];

    /// Every converter decodes liblzma's output back to the original bytes,
    /// and each fixture really exercises it: without the converter the
    /// LZMA2 payload differs from the original, and `lzma_rs` alone refuses
    /// the stream -- the #52 failure.
    #[test]
    fn every_bcj_filter_decodes_liblzma_output() {
        for (name, stream, plain) in FIXTURES {
            assert!(has_filters(stream), "{name}: fixture carries no filter");
            let mut plain_only = Vec::new();
            assert!(
                lzma_rs::xz_decompress(&mut std::io::BufReader::new(stream), &mut plain_only)
                    .is_err(),
                "{name}: lzma_rs alone decoded a filtered stream"
            );
            let unconverted = decompress_with(stream, 1 << 20, false).expect(name);
            assert_ne!(unconverted, plain, "{name}: the converter changed nothing");
            assert_eq!(decompress(stream, 1 << 20).expect(name), plain, "{name}");
        }
    }

    /// A damaged payload fails the block check rather than reading as data.
    #[test]
    fn a_flipped_byte_fails_the_check() {
        let (_, stream, _) = FIXTURES[0];
        let mut damaged = stream.to_vec();
        let at = damaged.len() - 40;
        damaged[at] ^= 0x01;
        assert!(
            decompress(&damaged, 1 << 20).is_err(),
            "a damaged stream was accepted"
        );
    }

    /// The output ceiling holds for a filtered stream as for a plain one.
    #[test]
    fn a_filtered_block_larger_than_max_out_is_refused() {
        let (_, stream, plain) = FIXTURES[0];
        assert!(decompress(stream, plain.len() - 1).is_err());
        assert!(decompress(stream, plain.len()).is_ok());
    }

    /// A plain LZMA2 stream is not routed here.
    #[test]
    fn a_plain_stream_has_no_filters() {
        let mut stream = Vec::new();
        lzma_rs::xz_compress(&mut std::io::BufReader::new(&b"plain"[..]), &mut stream).unwrap();
        assert!(!has_filters(&stream));
    }
}
