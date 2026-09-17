//! Pure-Rust SquashFS reader (read-only).
//!
//! SquashFS is a compressed, **read-only** filesystem — a peer format to
//! ext4/ntfs/erofs, not built on top of any of them. This crate reads
//! SquashFS 4.0 images (the only on-disk version `mksquashfs` from
//! squashfs-tools has emitted for over a decade) over any
//! [`fs_core::BlockRead`], and exposes a stable C ABI (`fs_squashfs_*`)
//! via [`capi`] so FFI consumers (an FSKit extension, C, Go) can link
//! `libfs_squashfs.a` and `#include "fs_squashfs.h"`.
//!
//! **Compression**: every standard SquashFS compressor is decoded —
//! gzip (zlib-wrapped DEFLATE, id 1), xz (`.xz` streams, id 4), lz4
//! (LZ4 block format, id 5), zstd (id 6), and lzo (LZO1X, id 3) via a
//! clean-room decoder. Legacy `lzma` (id 2) is best-effort. Unknown
//! ids surface as [`Error::UnsupportedCompression`].
//!
//! There is no write path: SquashFS cannot be modified in place (you
//! regenerate the whole image with `mksquashfs`), so the C ABI is the
//! read subset of the sister drivers' surface — no mkfs / create / write.
//!
//! Spec: the SquashFS on-disk format, as documented in
//! `linux/Documentation/filesystems/squashfs.rst` and the squashfs-tools
//! `squashfs_fs.h` field layout. Field names mirror those structs.
//! Independent clean-room implementation.
//!
//! Layout of the reader:
//! - [`superblock`] — parse + validate the 96-byte superblock
//! - [`decompress`] — codec dispatch (gzip / xz / lz4 / zstd / lzo)
//! - [`metablock`] — 8 KiB metadata-block reader + cross-block cursor
//! - [`table`] — indirect lookup tables (id, fragment, export)
//! - [`xattr`] — extended attributes: the id table and the name/value pairs
//! - [`inode`] — all SquashFS inode shapes (basic + extended)
//! - [`dir`] — directory listing parser
//! - [`fs`] — top-level handle: path lookup, dir listing, file/symlink read,
//!   extended attributes, inode-number resolution
//! - [`capi`] — C ABI exports matching `include/fs_squashfs.h`

#![deny(unsafe_op_in_unsafe_fn)]

pub mod decompress;
pub mod dir;
pub mod error;
pub mod fs;
pub mod inode;
pub mod metablock;
pub mod superblock;
pub mod table;
pub mod xattr;
mod xz;

// C ABI exports — surface defined in `include/fs_squashfs.h`.
pub mod capi;

pub use decompress::Compressor;
pub use dir::DirEntry;
pub use error::{Error, Result};
pub use fs::Filesystem;
pub use inode::{FileType, Inode};
pub use superblock::Superblock;
pub use xattr::XattrEntry;

// DOES THIS BUILD ACTUALLY TRAP AN ARITHMETIC OVERFLOW?
//
// Inline in `lib.rs` rather than a module of its own under `src/`: a
// separate file hangs off one `mod` line, and losing that line leaves
// the file present, uncompiled and asserting nothing, with no lint to
// say so. That has already happened once on a sibling repository's
// version of this fix -- a `git reset --hard` took the declaration, the
// file stayed, and seven assertions quietly stopped existing. Inline,
// there is no declaration to lose. It cannot live in `tests/` either:
// the question it answers is about the library target that the debug
// step in ci.yml builds, so it has to be part of that target.
#[cfg(test)]
mod overflow_checks {
    /// Set by the debug step in `ci.yml`, and by nothing else.
    ///
    /// The release steps must NOT set it: overflow checks are off there
    /// deliberately, because that is what ships. Setting it on a
    /// release run makes this module fail every time, which is the
    /// correct and loud response to that misconfiguration.
    const HANDSHAKE: &str = "EXPECT_OVERFLOW_CHECKS";

    /// Perform an overflow and report whether the program was stopped.
    ///
    /// The only question that matters, and the only one a text scan of
    /// `Cargo.toml` or the workflow cannot answer on its own: whichever
    /// spelling of "the checks are off" might exist -- a manifest key
    /// in any of its several spellings, a `CARGO_PROFILE_*` variable at
    /// step or job level, a `.cargo/config.toml` -- this asks the build
    /// directly instead of enumerating them and hoping the list is
    /// complete.
    fn this_build_traps_an_overflow() -> bool {
        // The hook is silenced so a deliberate panic does not print a
        // scary backtrace into a passing job's log.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let trapped = std::panic::catch_unwind(|| {
            let big = std::hint::black_box(u64::MAX);
            std::hint::black_box(big + 1);
        })
        .is_err();
        std::panic::set_hook(previous);
        trapped
    }

    /// When the gate says it built a profile that traps, check that it
    /// did.
    ///
    /// With `HANDSHAKE` unset this asserts nothing -- the shape of a
    /// test that passes because its fixture is missing -- and that is
    /// not guarded here because it cannot be: a build has no way to
    /// know whether it was supposed to be the checking one. It is
    /// guarded in `tests/ci_profile.rs`, which reads `ci.yml` and
    /// refuses if no `cargo test` there runs without `--release` while
    /// setting this variable.
    #[test]
    fn the_build_the_gate_asked_to_check_does_check() {
        let asked = match std::env::var(HANDSHAKE) {
            Ok(value) if !value.is_empty() => value,
            _ => return,
        };

        assert!(
            this_build_traps_an_overflow(),
            "{HANDSHAKE}={asked} was set, so this run is the one that is \
             supposed to panic on arithmetic overflow -- and it did not. \
             The debug step is running and blind, which is the exact \
             state it exists to rule out."
        );
    }
}

// THE README'S "CRATE LAYOUT" IS WHAT A NEW READER NAVIGATES BY.
//
// It listed an `lzo1x` module for several releases after the decoder
// moved out to the `am-lzo1x` crate (#12), so the clean-room claim was
// stated about code this crate no longer holds. A list written by hand
// beside a list the compiler owns drifts silently; this compares the
// two. See #51.
#[cfg(test)]
mod readme_layout {
    use std::collections::BTreeSet;

    /// The module names the README's `## Crate layout` section lists.
    fn listed() -> BTreeSet<String> {
        let readme = include_str!("../README.md");
        let section = readme
            .split("\n## Crate layout\n")
            .nth(1)
            .expect("README.md has no `## Crate layout` section");
        let section = section.split("\n## ").next().unwrap_or(section);
        section
            .lines()
            .filter_map(|l| l.strip_prefix("- `"))
            .filter_map(|l| l.split('`').next())
            .map(str::to_owned)
            .collect()
    }

    /// The module names this file declares.
    fn declared() -> BTreeSet<String> {
        include_str!("lib.rs")
            .lines()
            .filter_map(|l| l.strip_prefix("pub mod "))
            .filter_map(|l| l.strip_suffix(';'))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_readme_crate_layout_lists_exactly_the_declared_modules() {
        let declared = declared();
        // Control: a parser that found nothing would make both sets
        // empty and agree.
        assert!(
            declared.contains("capi") && declared.contains("decompress"),
            "could not read the module declarations out of lib.rs: {declared:?}"
        );
        let listed = listed();
        let phantom: Vec<_> = listed.difference(&declared).collect();
        let missing: Vec<_> = declared.difference(&listed).collect();
        assert!(
            phantom.is_empty() && missing.is_empty(),
            "README.md's crate layout lists modules that do not exist: {phantom:?}; \
             and omits modules that do: {missing:?}"
        );
    }
}
