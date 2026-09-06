//! The decompressed-metadata cache must not change any answer.
//!
//! A cache is only ever a speed argument, so the thing worth testing is
//! that it is *nothing but* a speed argument: the same image walked with
//! the cache switched off and switched on has to produce the same
//! listings, the same inode fields and the same file bytes, entry for
//! entry and byte for byte.
//!
//! The interesting case is the one the unit tests in `src/metablock.rs`
//! cannot reach: a real `mksquashfs` image, where a record genuinely
//! straddles a metadata-block boundary and a cursor therefore has to
//! splice a cached block onto a freshly-decompressed one. That splice is
//! the only place the cache touches the parse path, and it is where a
//! mistake would show up as wrong bytes rather than as slow ones.
//!
//! Needs `mksquashfs` to build the fixture, and skips without it.

mod common;
use common::{build_with_mksquashfs, dir, file, mksquashfs_available, pattern, symlink, Node};

use fs_squashfs::Filesystem;

/// Wide enough that a directory listing spans several metadata blocks
/// and deep enough that resolving a leaf reads the same blocks near the
/// root over and over — which is both what the cache is for and what
/// makes a bad splice visible.
fn fixture_tree() -> Node {
    let mut top = Vec::new();
    let top_names: Vec<String> = (0..4).map(|a| format!("top{a}")).collect();
    // Long names on purpose: a directory entry carries its name inline,
    // so long names push the listing past 8 KiB and force the boundary
    // case the cache has to get right.
    let mid_names: Vec<String> = (0..40)
        .map(|b| format!("directory-with-a-deliberately-long-name-{b:03}"))
        .collect();
    for top_name in &top_names {
        let mut mid = Vec::new();
        for (b, mid_name) in mid_names.iter().enumerate() {
            let leaf_names: Vec<String> = (0..6)
                .map(|c| format!("file-with-a-deliberately-long-name-{b:03}-{c}.bin"))
                .collect();
            let bodies: Vec<Vec<u8>> = (0..6).map(|c| pattern(700 + b * 7 + c)).collect();
            let mut leaf: Vec<(&str, Node)> = leaf_names
                .iter()
                .zip(bodies.iter())
                .map(|(n, body)| (n.as_str(), file(body)))
                .collect();
            leaf.push(("link", symlink("../..")));
            mid.push((mid_name.as_str(), dir(leaf)));
        }
        top.push((top_name.as_str(), dir(mid)));
    }
    dir(top)
}

/// Everything the driver will say about one path.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    path: String,
    inode_number: u32,
    permissions: u16,
    size: u64,
    names: Vec<String>,
    bytes: Vec<u8>,
    symlink: Vec<u8>,
}

fn survey(fs: &Filesystem, at: &str, out: &mut Vec<Seen>) {
    let Ok(inode) = fs.lookup_path(at) else {
        return;
    };
    let mut names = Vec::new();
    let mut bytes = Vec::new();
    let mut symlink = Vec::new();
    if inode.is_dir() {
        for e in fs.read_dir(&inode).expect("read_dir") {
            names.push(String::from_utf8_lossy(&e.name).to_string());
        }
    } else if inode.is_regular_file() {
        bytes = vec![0u8; inode.file_size as usize];
        let n = fs.read_file(&inode, 0, &mut bytes).expect("read_file");
        bytes.truncate(n);
    } else if inode.is_symlink() {
        symlink = fs.read_symlink_target(&inode).expect("readlink");
    }
    out.push(Seen {
        path: at.to_string(),
        inode_number: inode.inode_number,
        permissions: inode.permissions,
        size: inode.file_size,
        names: names.clone(),
        bytes,
        symlink,
    });
    for name in names {
        let child = if at == "/" {
            format!("/{name}")
        } else {
            format!("{at}/{name}")
        };
        survey(fs, &child, out);
    }
}

#[test]
fn the_cache_changes_the_speed_and_nothing_else() {
    if !mksquashfs_available() {
        eprintln!("mksquashfs not on PATH — skipping");
        return;
    }
    let image = build_with_mksquashfs("gzip", &fixture_tree());

    let cold = common::open_image_path(&image.path);
    cold.set_meta_cache_capacity(0);
    let mut without = Vec::new();
    survey(&cold, "/", &mut without);

    let warm = common::open_image_path(&image.path);
    let mut with = Vec::new();
    survey(&warm, "/", &mut with);

    assert!(
        without.len() > 500,
        "the fixture had only {} paths — too small to cross a metadata \
         block boundary, so this proves nothing",
        without.len()
    );
    assert_eq!(
        without.len(),
        with.len(),
        "the two passes saw a different number of paths"
    );
    for (a, b) in without.iter().zip(with.iter()) {
        assert_eq!(a, b, "the cache changed what the driver reported");
    }

    // A disabled cache reports nothing, rather than a 0% hit rate over
    // reads it was never consulted about.
    assert_eq!(
        cold.meta_cache_stats(),
        (0, 0, 0, 0),
        "the cache was disabled and still did something"
    );

    // And the enabled one has to be doing the job, or the equality above
    // is comparing two uncached passes and would pass no matter what.
    let (entries, capacity, hits, misses) = warm.meta_cache_stats();
    assert!(entries > 0 && entries <= capacity);
    assert!(
        hits > misses,
        "{hits} hits against {misses} misses — a walk of {} paths should \
         return to the same metadata blocks far more often than not",
        with.len()
    );
}
