//! The C header and the Rust `#[repr(C)]` structs describe the same
//! bytes -- checked by a C compiler, not by searching the header's text.
//!
//! A C consumer allocates and reads these structs from
//! `include/fs_squashfs.h`; the library writes them from `src/capi.rs`.
//! Nothing but convention kept the two in step, and the existing check
//! only looked for the array length `name[257]` in the header's text, so
//! a field whose type changed (`name_len` from `uint8_t` to `uint16_t` in
//! #56) or a field moved could pass while C read the name at the wrong
//! offset.
//!
//! This generates a C file of `_Static_assert`s from the Rust layout
//! (`size_of`, `align_of`, `offset_of!` of every field) and has the
//! system C compiler check them against the header. It fails rather
//! than skips in CI when there is no compiler.

use fs_squashfs::capi::*;
use std::mem::{align_of, offset_of, size_of};
use std::process::Command;

fn layout_asserts() -> String {
    let mut c = String::from("#include \"fs_squashfs.h\"\n#include <stddef.h>\n");
    macro_rules! check {
        ($ty:ident { $($field:ident),* $(,)? }) => {{
            let t = stringify!($ty);
            c.push_str(&format!(
                "_Static_assert(sizeof({t}) == {}, \"sizeof({t})\");\n",
                size_of::<$ty>()
            ));
            c.push_str(&format!(
                "_Static_assert(_Alignof({t}) == {}, \"alignof({t})\");\n",
                align_of::<$ty>()
            ));
            $(
                let f = stringify!($field);
                c.push_str(&format!(
                    "_Static_assert(offsetof({t}, {f}) == {}, \"offsetof({t}, {f})\");\n",
                    offset_of!($ty, $field)
                ));
                c.push_str(&format!(
                    "_Static_assert(sizeof((({t} *)0)->{f}) == {}, \"sizeof({t}.{f})\");\n",
                    std::mem::size_of_val(&unsafe { std::mem::zeroed::<$ty>() }.$field)
                ));
            )*
        }};
    }
    check!(fs_squashfs_attr_t {
        inode,
        mode,
        uid,
        gid,
        size,
        mtime,
        link_count,
        file_type,
        rdev
    });
    check!(fs_squashfs_dirent_t {
        inode,
        file_type,
        name_len,
        name
    });
    check!(fs_squashfs_volume_info_t {
        block_size,
        compression_id,
        compression_name,
        inode_count,
        fragment_count,
        id_count,
        bytes_used,
        mkfs_time,
        version_major,
        version_minor,
        flags,
    });
    check!(fs_squashfs_blockdev_cfg_t {
        read,
        context,
        size_bytes,
        block_size
    });
    c
}

#[test]
fn the_c_header_lays_out_every_abi_struct_as_the_library_does() {
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    if Command::new(&cc).arg("--version").output().is_err() {
        assert!(
            std::env::var_os("CI").is_none(),
            "no C compiler ({cc}) and CI is set, so the header's layout went unchecked"
        );
        eprintln!("no C compiler ({cc}) — skipping");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("layout.c");
    let asserts = layout_asserts();
    // Control: the generator produced the asserts it was meant to.
    assert_eq!(
        asserts.matches("_Static_assert").count(),
        2 * (9 + 4 + 11 + 4) + 2 * 4
    );
    std::fs::write(&src, &asserts).expect("write layout.c");
    let include = concat!(env!("CARGO_MANIFEST_DIR"), "/include");
    let out = Command::new(&cc)
        .args(["-std=c11", "-fsyntax-only", "-I", include])
        .arg(&src)
        .output()
        .expect("spawn the C compiler");
    assert!(
        out.status.success(),
        "include/fs_squashfs.h disagrees with the Rust layout:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
