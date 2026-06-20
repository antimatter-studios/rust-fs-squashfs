//! Clean-room LZO1X decompressor.
//!
//! SquashFS compression id 3 ("lzo") stores each block as an LZO1X
//! compressed stream (the `mksquashfs -comp lzo` default variant,
//! LZO1X-999 at encode time; all LZO1X-* encoders share one decode
//! grammar). This module decodes that stream.
//!
//! # Clean-room provenance
//!
//! This decoder was written WITHOUT reading, copying, or adapting any
//! existing LZO implementation — specifically NOT `liblzo2`/`minilzo`,
//! the Linux kernel `lib/lzo/*` C sources, `lzokay`, or any existing
//! Rust LZO crate. It is implemented purely from the PUBLICLY PUBLISHED
//! prose description of the LZO1X byte-stream grammar:
//!
//!   * "LZO stream format as understood by Linux's LZO decompressor",
//!     the format-description document distributed as
//!     `Documentation/staging/lzo.rst` in the Linux source tree (a prose
//!     spec of the on-the-wire token grammar, not the decompressor code).
//!
//! Only the documented token/instruction grammar below was used; no C
//! or Rust source for any LZO codec was consulted.
//!
//! # The grammar (paraphrased from the cited spec)
//!
//! The stream is a sequence of *instructions*. Each instruction is one
//! command byte, sometimes followed by extension/distance bytes, that
//! either emits a run of literal bytes copied straight from the input,
//! or a *match*: a back-reference that copies `len` bytes from `dist`
//! bytes earlier in the already-produced output. After most matches a
//! small number (0..=3) of trailing literals are copied immediately;
//! this count is the *state* carried into the next instruction and is
//! taken from the low 2 bits ("SS") of the match's distance operand.
//!
//! Lengths that would overflow the few bits available in the command
//! byte are extended by a run of zero bytes: each `0x00` adds 255, and
//! the terminating non-zero byte adds its own value (the "zero-run"
//! length extension).
//!
//! Instruction buckets, keyed by the command byte `t`:
//!
//! ```text
//! 0 0 0 0 L L L L  (t < 16)  -- three-way, keyed by the carried `state`:
//!     - state == 0: a literal run, len = 3 + (t==0 ? zero_run+15 : t).
//!                   Sets state = 4 afterwards (the "long-literals" sentinel).
//!     - state 1..=3 (the SS bits of the previous match): a 2-byte match,
//!                   dist = (next_byte << 2) + ((t >> 2) & 3) + 1.
//!     - state == 4: a 3-byte match (immediately after a long literal run),
//!                   dist = (next_byte << 2) + ((t >> 2) & 3) + 2049.
//!     In both match cases the new state is t & 3.
//! 0 0 0 1 H L L L  (16..=31)
//!     match, len = 2 + (LLL==0 ? zero_run+7 : LLL),
//!     dist = 16384 + (H << 14) + (LE16_operand >> 2);
//!     dist==16384 with no length bits is the END-OF-STREAM marker.
//! 0 0 1 L L L L L  (32..=63)
//!     match, len = 2 + (LLLLL==0 ? zero_run+31 : LLLLL),
//!     dist = (LE16_operand >> 2) + 1
//! 0 1 L D D D S S  (64..=127)
//!     match, len = 3 + ((t >> 5) & 1),
//!     dist = (next_byte << 3) + ((t >> 2) & 7) + 1
//! 1 L L D D D S S  (128..=255)
//!     match, len = 5 + ((t >> 5) & 3),
//!     dist = (next_byte << 3) + ((t >> 2) & 7) + 1
//! ```
//!
//! Stream bootstrap (first command byte only): a value >= 18 means a
//! leading literal run of `t - 17` bytes (with state = min(t-17, 4)); a
//! value of 17 is reserved as a bitstream-version marker we don't expect
//! from SquashFS. Values < 18 fall through to the normal grammar.
//!
//! The decoder is bounds-checked at every step and returns an error on
//! any malformed token rather than panicking, so it is safe to run on
//! untrusted images.

use crate::error::{Error, Result};

/// Decompress an LZO1X stream. `input` is the raw compressed block;
/// `max_out` is the UPPER BOUND on the decompressed length (the SquashFS
/// block size for data blocks, 8 KiB for metadata blocks). The true
/// decompressed length is determined by the stream's end-of-stream marker,
/// not by `max_out` — `max_out` only bounds the output buffer so a corrupt
/// stream can't make us allocate unboundedly. Returns the produced bytes.
pub fn decompress(input: &[u8], max_out: usize) -> Result<Vec<u8>> {
    let mut d = Decoder {
        input,
        ip: 0,
        out: Vec::with_capacity(max_out.min(1 << 20)),
        max_out,
    };
    d.run()?;
    Ok(d.out)
}

struct Decoder<'a> {
    input: &'a [u8],
    ip: usize,
    out: Vec<u8>,
    max_out: usize,
}

const E: Error = Error::BadMetadata("malformed LZO1X stream");

impl Decoder<'_> {
    #[inline]
    fn next(&mut self) -> Result<u8> {
        let b = *self.input.get(self.ip).ok_or(E)?;
        self.ip += 1;
        Ok(b)
    }

    /// Read a little-endian 16-bit operand (used by the long-distance
    /// match buckets; its low 2 bits double as the trailing-literal
    /// state, the upper 14 bits as the distance contribution).
    #[inline]
    fn next_le16(&mut self) -> Result<u16> {
        let lo = self.next()? as u16;
        let hi = self.next()? as u16;
        Ok(lo | (hi << 8))
    }

    /// Zero-run length extension: consume a run of `0x00` bytes (each
    /// worth 255) followed by one non-zero byte worth its own value.
    /// `base` is the length already accounted for by the command byte.
    /// Bounded so a corrupt all-zeros stream can't loop forever.
    fn extend_length(&mut self, base: usize) -> Result<usize> {
        let mut len = base;
        loop {
            let b = self.next()?;
            if b != 0 {
                return len.checked_add(b as usize).ok_or(E);
            }
            // Each zero byte adds 255. Cap the run length far below any
            // legitimate SquashFS block (<= 1 MiB) to reject garbage.
            len = len.checked_add(255).ok_or(E)?;
            if len > (1 << 24) {
                return Err(E);
            }
        }
    }

    /// Copy a back-reference of `len` bytes from `dist` bytes behind the
    /// current end of output. Overlapping copies are byte-at-a-time (the
    /// classic LZ77 self-referential run), so `dist < len` is legal.
    fn copy_match(&mut self, dist: usize, len: usize) -> Result<()> {
        if dist == 0 || dist > self.out.len() {
            return Err(E);
        }
        if self.out.len() + len > self.max_out {
            return Err(E);
        }
        let start = self.out.len() - dist;
        for src in start..start + len {
            let b = self.out[src];
            self.out.push(b);
        }
        Ok(())
    }

    /// Copy `n` literal bytes straight from the input to the output.
    fn copy_literals(&mut self, n: usize) -> Result<()> {
        if self.out.len() + n > self.max_out {
            return Err(E);
        }
        let end = self.ip.checked_add(n).ok_or(E)?;
        let slice = self.input.get(self.ip..end).ok_or(E)?;
        self.out.extend_from_slice(slice);
        self.ip = end;
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        // ---- stream bootstrap: leading literal run ----
        // The first command byte, when >= 18, is itself a literal-run
        // length of (t - 17). 17 is a reserved version marker.
        let mut state: usize;
        let first = *self.input.get(self.ip).ok_or(E)?;
        if first >= 18 {
            self.ip += 1;
            let n = (first - 17) as usize;
            self.copy_literals(n)?;
            state = n.min(4);
        } else {
            state = 0;
        }

        loop {
            // The stream is self-delimiting: a well-formed LZO1X stream
            // ends with the end-of-stream marker, decoded inside the match
            // arm below. Reaching the end of the input without a marker is
            // malformed (`next()` returns an error).
            let t = self.next()?;

            if t >= 16 {
                // Match instruction. Decode by the top set bit.
                let (len, dist, new_state) = if t >= 128 {
                    // 1 L L D D D S S — 5..=8 byte match, short distance.
                    let h = self.next()? as usize;
                    let len = 5 + ((t >> 5) & 3) as usize;
                    let dist = (h << 3) + ((t >> 2) & 7) as usize + 1;
                    (len, dist, (t & 3) as usize)
                } else if t >= 64 {
                    // 0 1 L D D D S S — 3..=4 byte match, short distance.
                    let h = self.next()? as usize;
                    let len = 3 + ((t >> 5) & 1) as usize;
                    let dist = (h << 3) + ((t >> 2) & 7) as usize + 1;
                    (len, dist, (t & 3) as usize)
                } else if t >= 32 {
                    // 0 0 1 L L L L L — match, distance 1..=16384.
                    let base = (t & 31) as usize;
                    let len = if base == 0 {
                        self.extend_length(31)? + 2
                    } else {
                        base + 2
                    };
                    let op = self.next_le16()? as usize;
                    let dist = (op >> 2) + 1;
                    (len, dist, op & 3)
                } else {
                    // 0 0 0 1 H L L L — match, distance 16384..=49151,
                    // plus the end-of-stream marker.
                    let h = ((t >> 3) & 1) as usize;
                    let base = (t & 7) as usize;
                    let len = if base == 0 {
                        self.extend_length(7)? + 2
                    } else {
                        base + 2
                    };
                    let op = self.next_le16()? as usize;
                    let dist = 16384 + (h << 14) + (op >> 2);
                    // EOS: the canonical marker is `11 00 00`
                    // (t=0x11, LE16=0x0000) -> dist == 16384, len == 3.
                    if op >> 2 == 0 && h == 0 {
                        return Ok(());
                    }
                    (len, dist, op & 3)
                };

                self.copy_match(dist, len)?;
                state = new_state;
            } else {
                // t < 16: the `0 0 0 0 ...` bucket, three-way by `state`
                // (the number of literals the PREVIOUS instruction left
                // pending, where 4 is the sentinel meaning "a long literal
                // run just happened").
                if state == 0 {
                    // Long literal run. t==0 triggers the zero-run
                    // extension; otherwise the run is t + 3 bytes long.
                    // Afterwards state = 4 (the sentinel).
                    let n = if t == 0 {
                        self.extend_length(15)? + 3
                    } else {
                        t as usize + 3
                    };
                    self.copy_literals(n)?;
                    state = 4;
                    continue;
                }
                // Both remaining cases are a short match whose command byte
                // is `0 0 0 0 D D S S`: DD = (t >> 2) & 3 contributes to the
                // distance, SS = t & 3 becomes the next trailing-literal
                // state. One more byte H extends the distance.
                //   state 1..=3 -> a 2-byte match,  dist = (H<<2)+DD+1
                //   state 4     -> a 3-byte match,  dist = (H<<2)+DD+2049
                let h = self.next()? as usize;
                let dd = ((t >> 2) & 3) as usize;
                let (len, dist) = if state == 4 {
                    (3, (h << 2) + dd + 2049)
                } else {
                    (2, (h << 2) + dd + 1)
                };
                self.copy_match(dist, len)?;
                state = (t & 3) as usize;
            }

            // After a match, copy `state` trailing literals immediately.
            if state > 0 {
                self.copy_literals(state)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A literal-only stream: bootstrap byte >= 18 emits (t-17) literals,
    /// then the EOS marker. Round-trips to exactly those literal bytes.
    #[test]
    fn literal_only_via_bootstrap() {
        // 4 literal bytes: bootstrap t = 17 + 4 = 21, then "abcd",
        // then EOS marker 0x11 0x00 0x00.
        let stream = [21u8, b'a', b'b', b'c', b'd', 0x11, 0x00, 0x00];
        let out = decompress(&stream, 4).unwrap();
        assert_eq!(&out, b"abcd");
    }

    /// A literal run encoded via the `t < 16` literal bucket (state 0,
    /// t = len - 3). 5 literals -> t = 2.
    #[test]
    fn literal_run_small_bucket() {
        // First byte < 18 so no bootstrap run; state starts 0.
        // t = 2 -> 5 literals "hello", then EOS.
        let stream = [2u8, b'h', b'e', b'l', b'l', b'o', 0x11, 0x00, 0x00];
        let out = decompress(&stream, 5).unwrap();
        assert_eq!(&out, b"hello");
    }

    /// A back-reference that repeats earlier output, exercising
    /// `copy_match` including an overlapping (dist < len) run.
    #[test]
    fn match_repeats_prior_output() {
        // Bootstrap: 1 literal 'A' (t = 18). State after bootstrap = 1.
        // With state==1, the next t<16 byte is a short 2-byte match, not
        // a literal — so instead use a `001 LLLLL` long match to repeat.
        //
        // Build: bootstrap 'A' via t=18 path would set state=1; to keep
        // the test on the literal path, use the literal bucket directly.
        //
        // Sequence:
        //   t=0x00 -> literal run, zero-run base 15 + ext. Use t=0x01? No:
        //   simpler — emit 1 literal 'A' through t<16 literal bucket needs
        //   len>=3. So emit 3 literals "AAA" with t=0 path is awkward;
        //   instead emit "Aaa" via t = 0 (len 3 + zero-run). Keep it
        //   concrete: t=0x00 then zero-run terminating byte to make len=3.
        //
        // Easiest exact stream: literal bucket t=0 with extend -> len 3,
        // literals "xyz"; then a 0 0 1 match copying dist=3,len=5 to
        // produce "xyzxy", then EOS.
        //
        // t=0: literal, base 15 + extend_length(15). extend reads bytes:
        //   first non-zero byte b adds b; but base passed is 15, and code
        //   does extend_length(15)? + 3. To get len=3 we need the literal
        //   bucket with t in 1..=12. Use t=0x00? that gives >= 18. Use
        //   t=0 is wrong here; pick t=0 only for long runs. Use t=0?? ->
        //   Instead use t = (len-3) = 0 won't work (0 means extend). So
        //   choose len=3 via t=0? no. len=4 via t=1.
        //
        // Use t=1 -> 4 literals "wxyz".
        let mut stream = vec![1u8, b'w', b'x', b'y', b'z'];
        // 0 0 1 match: t = 0x20 | len_bits. len = base + 2; want len=4 ->
        // base=2 -> t = 0x22. dist operand LE16: dist = (op>>2)+1; want
        // dist=4 -> op>>2 = 3 -> op = 12 (0x0C), state bits (op&3)=0.
        stream.extend_from_slice(&[0x22, 0x0C, 0x00]);
        // EOS marker.
        stream.extend_from_slice(&[0x11, 0x00, 0x00]);
        // Expected: "wxyz" + copy(dist=4,len=4) starting at output[0] ->
        // "wxyz" again -> "wxyzwxyz".
        let out = decompress(&stream, 8).unwrap();
        assert_eq!(&out, b"wxyzwxyz");
    }

    /// Overlapping match: dist=1, len=4 turns one byte into a run.
    #[test]
    fn overlapping_run() {
        // 1 literal 'Q' via t=1 -> needs 4 literals; instead t for 4
        // literals is fine, then overlap-copy the last byte. Simpler:
        // emit "QQQQ" by literal 'Q' x? Use literal run len 4 then a
        // 0 0 1 match dist=1 len=4.
        let mut stream = vec![1u8, b'Q', b'R', b'S', b'T'];
        // dist=1 len=4: t=0x22 (len base 2 -> len 4), op: dist=1 ->
        // op>>2 = 0 -> op=0, state 0.
        stream.extend_from_slice(&[0x22, 0x00, 0x00]);
        stream.extend_from_slice(&[0x11, 0x00, 0x00]);
        // "QRST" then copy dist=1 len=4 from last byte 'T' -> "TTTT".
        let out = decompress(&stream, 8).unwrap();
        assert_eq!(&out, b"QRSTTTTT");
    }

    #[test]
    fn truncated_stream_errors() {
        // Claims a literal run longer than the input provides.
        let stream = [21u8, b'a', b'b'];
        assert!(decompress(&stream, 4).is_err());
    }

    #[test]
    fn match_before_any_output_errors() {
        // A match instruction as the very first token has no prior output
        // to copy from -> dist out of range.
        let stream = [0x22u8, 0x00, 0x00];
        assert!(decompress(&stream, 4).is_err());
    }
}
