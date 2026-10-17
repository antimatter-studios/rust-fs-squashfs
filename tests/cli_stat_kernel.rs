//! Stat metadata on reference-built images, checked against the mounted
//! kernel inode and the reference unpacker in the harness VM. No parser
//! or fixture builder from this crate supplies the expected metadata.

mod cli_support;

use cli_support::*;
use fs_squashfs_test_support::{guest_kernel_read, mksquashfs_from_guest_tree, ScratchDir};
use std::os::unix::ffi::OsStrExt;

const STAGE: &str = r#"
mkdir dir
chmod 1777 dir
head -c 5000 /dev/urandom > file
chmod 0640 file
chown 1234:5678 file
ln file hard
setfattr -n user.stat -v oracle file
head -c 4096 /dev/urandom > one
: > empty
ln -s file link
mkfifo fifo
mknod chr c 259 300
mknod blk b 7 300
python3 -c 'import socket; s=socket.socket(socket.AF_UNIX); s.bind("sock")'
printf raw > $'raw-\xff'
truncate -s 4096 $'raw-\xff'
"#;

const PATHS: &[&[u8]] = &[
    b"/",
    b"/dir",
    b"/file",
    b"/hard",
    b"/one",
    b"/empty",
    b"/link",
    b"/fifo",
    b"/chr",
    b"/blk",
    b"/sock",
    b"/raw-\xff",
];

// Paths travel as hex so neither invalid UTF-8 nor whitespace changes
// which inode the oracle stats. unsquashfs extracts to the guest's own
// disk; Linux also reads the original image through a read-only mount.
const REPORT: &str = r#"
work="$(mktemp -d /var/tmp/squashfs-stat.XXXXXX)"
trap 'rm -rf "$work"' EXIT
unsquashfs -no-progress -d "$work/tree" "$IMG" > "$work/unpack.log" 2>&1 || { cat "$work/unpack.log" >&2; exit 1; }
python3 - "$MNT" "$work/tree" <<'PY'
import os, stat, sys
for rel in [b'', b'dir', b'file', b'hard', b'one', b'empty', b'link', b'fifo', b'chr', b'blk', b'sock', b'raw-\xff']:
    mounted = os.fsencode(sys.argv[1]) + b'/' + rel
    extracted = os.fsencode(sys.argv[2]) + b'/' + rel
    a, b = os.lstat(mounted), os.lstat(extracted)
    # Extraction preserves these fields. Directory sizes and inode
    # numbers belong to the destination filesystem, so use the mount.
    for key in ['st_mode', 'st_uid', 'st_gid', 'st_nlink', 'st_mtime_ns', 'st_rdev']:
        assert getattr(a, key) == getattr(b, key), (rel, key, a, b)
    if not stat.S_ISDIR(a.st_mode):
        assert a.st_size == b.st_size, (rel, a, b)
    target = os.readlink(mounted) if stat.S_ISLNK(a.st_mode) else b''
    if target:
        assert target == os.readlink(extracted)
    xattrs = os.listxattr(mounted, follow_symlinks=False)
    assert sorted(xattrs) == sorted(os.listxattr(extracted, follow_symlinks=False)), rel
    print('|'.join(map(str, [(b'/' + rel).hex(), a.st_ino, a.st_mode, a.st_uid, a.st_gid,
          a.st_nlink, a.st_size, a.st_mtime_ns // 1000000000,
          os.major(a.st_rdev), os.minor(a.st_rdev), target.hex(), len(xattrs)])))
PY
"#;

fn kind(mode: u32) -> &'static str {
    match mode & 0o170000 {
        0o100000 => "file",
        0o040000 => "dir",
        0o120000 => "symlink",
        0o020000 => "char",
        0o060000 => "block",
        0o010000 => "fifo",
        0o140000 => "socket",
        _ => panic!("unexpected oracle mode {mode:o}"),
    }
}

fn check(fragments: bool) {
    let dir = ScratchDir::new(if fragments {
        "stat-fragments"
    } else {
        "stat-blocks"
    });
    let image = dir.join("stat.sqfs");
    let layout = if fragments {
        "-always-use-fragments"
    } else {
        "-no-fragments"
    };
    let built = mksquashfs_from_guest_tree(
        &image,
        STAGE,
        &[
            "-noappend",
            "-no-progress",
            "-b",
            "4096",
            "-all-time",
            "1760000000",
            layout,
        ],
    );
    assert!(built.status.success(), "mksquashfs: {}", stderr(&built));
    let image = image.display().to_string();
    let report = guest_kernel_read(&image, REPORT);
    assert!(
        report.status.success(),
        "oracle: {}{}",
        stdout(&report),
        stderr(&report)
    );
    let report = stdout(&report);
    assert_eq!(report.lines().count(), PATHS.len(), "{report}");
    for (line, path) in report.lines().zip(PATHS) {
        let f: Vec<_> = line.split('|').collect();
        assert_eq!(f.len(), 12, "{line}");
        let hex: String = path.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(f[0], hex, "oracle path order");
        let ours = stdout(&ok(tool("fs.squashfs")
            .args([&image, "stat"])
            .arg(std::ffi::OsStr::from_bytes(path))));
        let field = |key: &str| json_field(&ours, key);
        let mode: u32 = f[2].parse().unwrap();
        for (key, value) in [
            ("inode", f[1]),
            ("uid", f[3]),
            ("gid", f[4]),
            ("nlink", f[5]),
            ("size", f[6]),
            ("mtime", f[7]),
        ] {
            assert_eq!(field(key), value, "{hex}: {key}\n{ours}\n{line}");
        }
        assert_eq!(field("mode"), format!("{:04o}", mode & 0o7777));
        assert_eq!(field("type"), kind(mode));
        for key in [
            "allocated_bytes",
            "atime",
            "atime_nsec",
            "mtime_nsec",
            "ctime",
            "ctime_nsec",
            "btime",
            "btime_nsec",
        ] {
            assert_eq!(field(key), "null", "{hex}: {key}\n{ours}");
        }
        if std::str::from_utf8(path).is_err() {
            assert_eq!(field("path_hex"), hex);
        } else {
            assert_eq!(field("path"), String::from_utf8_lossy(path));
        }
        match kind(mode) {
            "char" | "block" => {
                assert_eq!(field("major"), f[8]);
                assert_eq!(field("minor"), f[9]);
            }
            "symlink" => assert_eq!(field("target"), "file"),
            _ => assert!(!ours.contains("\"target\":")),
        }
        let inode_type: u16 = field("inode_type").parse().unwrap();
        assert_eq!(
            (inode_type - 1) % 7 + 1,
            match kind(mode) {
                "dir" => 1,
                "file" => 2,
                "symlink" => 3,
                "block" => 4,
                "char" => 5,
                "fifo" => 6,
                "socket" => 7,
                _ => unreachable!(),
            }
        );
        assert_eq!(field("xattr_index") != "null", f[11] != "0");
        if path == b"/file" || path == b"/hard" {
            assert_eq!(inode_type, 9, "hardlinked file with a reference xattr");
            assert_eq!(field("xattr_index"), "0", "the only reference xattr set");
            assert_eq!(field("block_count"), if fragments { "1" } else { "2" });
            assert_eq!(
                field("fragment_offset"),
                if fragments { "0" } else { "null" }
            );
        }
        if path == b"/one" {
            assert_eq!(inode_type, 2, "basic file");
            assert_eq!(field("block_count"), "1");
        }
        if !fragments || !(path == b"/file" || path == b"/hard") {
            assert_eq!(field("fragment_index"), "null");
            assert_eq!(field("fragment_offset"), "null");
        } else {
            assert_eq!(
                field("fragment_index"),
                "0",
                "one packed reference fragment"
            );
        }
        if kind(mode) != "file" {
            assert_eq!(field("block_count"), "null");
        }
    }
}

#[test]
fn stat_matches_the_kernel_and_unsquashfs_with_fragments() {
    check(true);
}

#[test]
fn stat_matches_the_kernel_and_unsquashfs_without_fragments() {
    check(false);
}
