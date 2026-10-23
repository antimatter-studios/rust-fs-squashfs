//! `fs.squashfs export --recursive` through the shared tree transfer (#195),
//! judged by what is not this crate: the Linux kernel mounting the image in
//! the harness VM, and unsquashfs extracting it there.
//!
//! The trees are staged by the guest and built by `mksquashfs`: one at
//! least 32 directories deep with an empty file, a multi-block file, three
//! symlinks and a fifo; one with hard links, symlinks whose targets escape
//! DEST or are not UTF-8, and stored modification times; and a small one
//! for the depth bound, merging, replacement and `--offset`. Every exported
//! file's SHA-256, every type and every symlink target must be the
//! kernel's and unsquashfs's, and a file whose data cannot be read fails
//! alone, without publishing or replacing anything.

mod cli_support;

use cli_support::*;
use fs_core::FileDevice;
use fs_squashfs::Filesystem;
use fs_squashfs_test_support::{
    assert_unsquashfs_walks, guest_kernel_read, guest_kernel_report, mksquashfs_from_guest_tree,
    oracle, sha256_hex, ScratchDir,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// How deep the first tree goes below `tree`.
const DEPTH: usize = 33;

/// The report's item after its path key, up to the next item's path key.
#[track_caller]
fn item<'a>(report: &'a str, path: &str) -> &'a str {
    let needle = format!("\"path\": \"{path}\"");
    let start = report
        .find(&needle)
        .unwrap_or_else(|| panic!("no item for {path} in:\n{report}"));
    let rest = &report[start + needle.len()..];
    &rest[..rest.find("\"path\": ").unwrap_or(rest.len())]
}

/// `fs.squashfs IMAGE GLOBAL... export --recursive FLAGS... /tree DEST`.
fn export_tree(image: &Path, global: &[&str], flags: &[&str], dest: &Path) -> std::process::Output {
    tool("fs.squashfs")
        .arg(image)
        .args(global)
        .args(["export", "--recursive"])
        .args(flags)
        .arg("/tree")
        .arg(dest)
        .output()
        .unwrap()
}

/// Build `image` from a tree the guest stages, and check unsquashfs walks it.
fn build(image: &Path, stage: &str) {
    let built =
        mksquashfs_from_guest_tree(image, stage, &["-b", "4096", "-noappend", "-no-progress"]);
    assert!(built.status.success(), "mksquashfs: {}", stderr(&built));
    assert_unsquashfs_walks(image.to_str().unwrap(), "tree export source");
}

/// The `type`, `sha256` and `target` rows of a `kind<TAB>path<TAB>value`
/// report, keyed by `(kind, path)`.
fn rows(report: &str) -> BTreeMap<(String, String), String> {
    report
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let kind = fields.next()?;
            let path = fields.next()?;
            let value = fields.next()?;
            ["type", "sha256", "target"]
                .contains(&kind)
                .then(|| ((kind.to_string(), path.to_string()), value.to_string()))
        })
        .collect()
}

/// unsquashfs's own reading of the image, in the rows the kernel report
/// uses: extracted on the guest's disk, where the ownership it restores
/// means something, and only the report crosses back. One line, so the
/// evidence the oracle prints stays one line.
const UNSQUASHFS_REPORT: &str =
    "set -euo pipefail; w=$(mktemp -d /var/tmp/fs-squashfs-tree.XXXXXX); \
     unsquashfs -f -no-xattrs -d \"$w/x\" \"$1\" >/dev/null; cd \"$w/x\"; \
     find . -mindepth 1 -printf '%P\\n' | sort | while read -r p; do \
     t=$(stat -c %F \"$p\" | tr ' ' -); printf 'type\\t%s\\t%s\\n' \"$p\" \"$t\"; \
     case $t in regular-file|regular-empty-file) \
     printf 'sha256\\t%s\\t%s\\n' \"$p\" \"$(sha256sum \"$p\" | cut -d' ' -f1)\";; \
     symbolic-link) printf 'target\\t%s\\t%s\\n' \"$p\" \"$(readlink \"$p\")\";; esac; done; \
     rm -rf \"$w\"";

fn unsquashfs_rows(image: &Path) -> BTreeMap<(String, String), String> {
    let out = oracle("bash")
        .args(["-c", UNSQUASHFS_REPORT, "unsquashfs-report"])
        .arg(image.to_str().unwrap())
        .output();
    assert!(out.status.success(), "unsquashfs report: {}", stderr(&out));
    rows(&stdout(&out))
}

/// The same rows for a tree exported to `root`, as `tree/...`.
fn exported_rows(root: &Path) -> BTreeMap<(String, String), String> {
    fn walk(dir: &Path, rel: &str, out: &mut BTreeMap<(String, String), String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = format!("{rel}/{}", entry.file_name().to_string_lossy());
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let kind = if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                out.insert(
                    ("target".into(), rel.clone()),
                    target.to_string_lossy().into_owned(),
                );
                "symbolic-link"
            } else if meta.is_dir() {
                walk(&path, &rel, out);
                "directory"
            } else if meta.is_file() {
                let bytes = std::fs::read(&path).unwrap();
                out.insert(("sha256".into(), rel.clone()), sha256_hex(&bytes));
                if bytes.is_empty() {
                    "regular-empty-file"
                } else {
                    "regular-file"
                }
            } else {
                "other"
            };
            out.insert(("type".into(), rel), kind.into());
        }
    }
    let mut out = BTreeMap::new();
    out.insert(("type".into(), "tree".into()), "directory".into());
    walk(root, "tree", &mut out);
    out
}

/// No staging file is left beside `name` in `dir`.
#[track_caller]
fn assert_no_staging(dir: &Path, name: &str) {
    let stem = format!(".{name}.");
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!entry.starts_with(&stem), "staging file remained: {entry}");
    }
}

const DEEP_STAGE: &str = "mkdir tree\ncd tree\n: > empty\n\
     python3 - <<'PY'\n\
     with open('big', 'wb') as f:\n    f.write(bytes(range(256)) * 12288 + b'!')\nPY\n\
     p=.\nfor i in $(seq 1 33); do\n\
     p=\"$p/d$i\"\nmkdir \"$p\"\nprintf 'file %s' \"$i\" > \"$p/f$i\"\ndone\n\
     ln -s big link\nln -s absent dangling\nln -s ../.. d1/up\nmkfifo pipe\n";

#[test]
fn recursive_export_matches_the_kernel_and_unsquashfs_and_a_damaged_file_fails_alone() {
    let dir = ScratchDir::new("cli-export-tree");
    let image = dir.join("source.sqfs");
    build(&image, DEEP_STAGE);
    let before = std::fs::read(&image).unwrap();
    let kernel = guest_kernel_report(image.to_str().unwrap(), "tree export reference");
    assert_eq!(
        kernel
            .get(&("size".to_string(), "tree/big".to_string()))
            .unwrap(),
        "3145729"
    );
    let kernel: BTreeMap<_, _> = kernel
        .into_iter()
        .filter(|((kind, _), _)| ["type", "sha256", "target"].contains(&kind.as_str()))
        .collect();
    let sums: Vec<_> = kernel
        .iter()
        .filter(|((kind, _), _)| kind == "sha256")
        .collect();
    assert_eq!(sums.len(), DEPTH + 2, "the kernel digested every file");
    assert_eq!(
        unsquashfs_rows(&image),
        kernel,
        "unsquashfs and the kernel read the same tree"
    );

    // Without --recursive a directory is refused, as before.
    let dest = dir.join("exported");
    let plain = tool("fs.squashfs")
        .arg(&image)
        .args(["export", "/tree"])
        .arg(&dest)
        .output()
        .unwrap();
    assert_eq!(plain.status.code(), Some(1), "{}", stderr(&plain));
    assert!(
        !dest.exists(),
        "without --recursive directories stay refused"
    );

    // Every file and directory, each reported; symlinks refused without
    // --preserve=links, and the fifo skipped.
    let out = export_tree(&image, &[], &[], &dest);
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
    assert_eq!(
        json_field(&report, "completed"),
        (DEPTH * 2 + 3).to_string()
    );
    assert_eq!(json_field(&report, "skipped"), "1", "{report}");
    assert_eq!(json_field(&report, "refused"), "3", "{report}");
    assert_eq!(json_field(&report, "failed"), "0", "{report}");
    for ((_, path), want) in &sums {
        let bytes = std::fs::read(dest.join(path.strip_prefix("tree/").unwrap())).unwrap();
        assert_eq!(&sha256_hex(&bytes), *want, "{path}: exported versus kernel");
        let entry = item(&report, &format!("/{path}"));
        assert!(entry.contains("\"status\": \"completed\""), "{entry}");
        assert!(entry.contains(want.as_str()), "{entry}");
    }
    let mut deepest = dest.clone();
    for i in 1..=DEPTH {
        deepest.push(format!("d{i}"));
        assert!(deepest.is_dir(), "directory {i} was exported");
        let path = format!("/tree/{}", deepest.strip_prefix(&dest).unwrap().display());
        assert!(item(&report, &path).contains("\"status\": \"completed\""));
    }
    for path in ["link", "dangling", "d1/up", "pipe"] {
        let entry = item(&report, &format!("/tree/{path}"));
        let status = if path == "pipe" { "skipped" } else { "refused" };
        assert!(
            entry.contains(&format!("\"status\": \"{status}\"")),
            "{entry}"
        );
        if path != "pipe" {
            assert!(entry.contains("--preserve=links"), "{entry}");
        }
        assert!(std::fs::symlink_metadata(dest.join(path)).is_err());
    }

    // An existing file is refused without --replace and kept.
    let kept = dest.join("big");
    std::fs::write(&kept, b"keep me").unwrap();
    let again = export_tree(&image, &[], &[], &dest);
    assert_eq!(again.status.code(), Some(1), "{}", stderr(&again));
    assert!(item(&stdout(&again), "/tree/big").contains("\"status\": \"refused\""));
    assert_eq!(std::fs::read(&kept).unwrap(), b"keep me");

    // With --preserve=links every symlink is recreated, its target never
    // followed: the exported tree is the kernel's and unsquashfs's, the
    // fifo apart.
    let linked = dir.join("linked");
    let out = export_tree(&image, &[], &["--preserve=links"], &linked);
    let report = stdout(&out);
    assert_eq!(
        json_field(&report, "completed"),
        (DEPTH * 2 + 6).to_string(),
        "{report}"
    );
    assert_eq!(json_field(&report, "refused"), "0", "{report}");
    assert_eq!(json_field(&report, "failed"), "0", "{report}");
    let mut want = kernel.clone();
    want.remove(&("type".to_string(), "tree/pipe".to_string()));
    assert_eq!(exported_rows(&linked), want, "{report}");
    assert_eq!(
        std::fs::read(&image).unwrap(),
        before,
        "export changed its source"
    );

    // A damaged data block in `big`, past the first transfer chunk: `big`
    // fails, and everything beside it is still exported.
    let damaged = dir.join("damaged.sqfs");
    let fs = Filesystem::open(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
    let inode = fs.lookup_path("/tree/big").unwrap();
    let index = 257;
    let size = inode.block_sizes[index];
    assert_eq!(size & (1 << 24), 0, "the damaged block must be compressed");
    let at = inode.blocks_start as usize
        + inode.block_sizes[..index]
            .iter()
            .map(|size| (size & 0x00ff_ffff) as usize)
            .sum::<usize>();
    let mut bytes = before.clone();
    bytes[at..at + (size & 0x00ff_ffff) as usize].fill(0);
    std::fs::write(&damaged, bytes).unwrap();
    let reference = oracle("unsquashfs")
        .args(["-cat", damaged.to_str().unwrap(), "tree/big"])
        .output();
    assert!(
        !reference.status.success(),
        "unsquashfs accepted the damaged block"
    );
    let partial = dir.join("partial");
    for replace in [false, true] {
        let flags: &[&str] = if replace { &["--replace"] } else { &[] };
        let out = export_tree(&damaged, &[], flags, &partial);
        let report = stdout(&out);
        assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
        assert_eq!(json_field(&report, "failed"), "1", "{report}");
        let failed = item(&report, "/tree/big");
        assert!(failed.contains("\"status\": \"failed\""), "{failed}");
        for ((_, path), want) in &sums {
            if *path == "tree/big" {
                continue;
            }
            let bytes = std::fs::read(partial.join(path.strip_prefix("tree/").unwrap())).unwrap();
            assert_eq!(&sha256_hex(&bytes), *want, "{path}: exported versus kernel");
            assert!(item(&report, &format!("/{path}")).contains("\"status\": \"completed\""));
        }
        if replace {
            assert_eq!(
                std::fs::read(partial.join("big")).unwrap(),
                b"original destination",
                "a failed replacement kept the original bytes"
            );
        } else {
            assert!(
                std::fs::symlink_metadata(partial.join("big")).is_err(),
                "an incomplete file was published"
            );
            std::fs::write(partial.join("big"), b"original destination").unwrap();
        }
        assert_no_staging(&partial, "big");
    }
}

#[test]
fn recursive_export_preserves_the_kernels_links_and_stored_times() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    const STAGE: &str = r#"
mkdir -p tree/sub
printf 'linked content\n' > tree/file
ln tree/file tree/hard
ln tree/file tree/sub/hard2
python3 - <<'PY'
import os
os.symlink(b'file', b'tree/short')
os.symlink(b'long/' * 40 + b'absent', b'tree/long')
os.symlink(b'../absent-\xff\n', b'tree/raw')
os.symlink(b'../..', b'tree/sub/up')
os.utime('tree/file', ns=(981173106123456789, 1234567890987654321))
os.utime('tree/sub', ns=(1000000000500000000, 1100000000250000000))
os.utime('tree', ns=(1000000000123456789, 1200000000123456789))
PY
"#;
    const REPORT: &str = r#"
cd "$MNT"
python3 - <<'PY'
import os
for name in ['file', 'hard', 'sub/hard2', 'sub', '.']:
    s = os.lstat('tree/' + name)
    print('M', name, s.st_ino, s.st_nlink, s.st_mtime_ns)
for name in ['short', 'long', 'raw', 'sub/up']:
    print('L', name, os.readlink(b'tree/' + name.encode()).hex())
PY
"#;

    let dir = ScratchDir::new("cli-export-tree-preserve");
    let image = dir.join("source.sqfs");
    build(&image, STAGE);
    let before = std::fs::read(&image).unwrap();
    let kernel = guest_kernel_read(image.to_str().unwrap(), REPORT);
    assert!(
        kernel.status.success(),
        "{}{}",
        stdout(&kernel),
        stderr(&kernel)
    );
    let kernel = stdout(&kernel);
    let rows: Vec<Vec<&str>> = kernel
        .lines()
        .map(|line| line.split(' ').collect())
        .collect();
    assert_eq!(rows.len(), 9, "{kernel}");

    // Without --preserve=links symlinks are refused and hard links copied.
    let plain = dir.join("plain");
    let out = export_tree(&image, &[], &[], &plain);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let report = stdout(&out);
    for name in ["short", "long", "raw", "sub/up"] {
        let entry = item(&report, &format!("/tree/{name}"));
        assert!(entry.contains("\"status\": \"refused\""), "{entry}");
        assert!(entry.contains("--preserve=links"), "{entry}");
        assert!(std::fs::symlink_metadata(plain.join(name)).is_err());
    }
    assert_ne!(
        std::fs::metadata(plain.join("file")).unwrap().ino(),
        std::fs::metadata(plain.join("hard")).unwrap().ino()
    );

    let dest = dir.join("preserved");
    let out = export_tree(&image, &[], &["--preserve=links,times"], &dest);
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{report}{}", stderr(&out));
    assert_eq!(json_field(&report, "completed"), "9", "{report}");
    assert_eq!(json_field(&report, "failed"), "0", "{report}");
    let linked = std::fs::metadata(dest.join("file")).unwrap();
    let first = &rows[0];
    assert_eq!((first[0], first[1]), ("M", "file"));
    assert_eq!(first[3], "3", "the kernel must see three hard links");
    for row in &rows {
        let name = row[1];
        let local = dest.join(name);
        if row[0] == "L" {
            let target = std::fs::read_link(&local).unwrap();
            let hex: String = target
                .as_os_str()
                .as_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(hex, row[2], "{name}: kernel readlink bytes");
            let entry = item(&report, &format!("/tree/{name}"));
            assert!(
                entry.contains("symbolic link times not preserved"),
                "{entry}"
            );
            if name == "raw" || name == "sub/up" {
                assert!(entry.contains("target escapes DEST"), "{entry}");
            }
            continue;
        }
        assert_eq!(row[0], "M");
        // SquashFS stores whole seconds; the kernel's nanoseconds are zero.
        let host = std::fs::metadata(&local).unwrap();
        assert_eq!(
            i128::from(host.mtime()) * 1_000_000_000 + i128::from(host.mtime_nsec()),
            row[4].parse::<i128>().unwrap(),
            "{name}: modification time versus kernel"
        );
        let source = if name == "." {
            "/tree".to_string()
        } else {
            format!("/tree/{name}")
        };
        // SquashFS stores no access time, and core says so.
        let entry = item(&report, &source);
        assert!(entry.contains("one source time unavailable"), "{entry}");
        if host.is_file() {
            assert_eq!(row[2], first[2], "kernel hard-link identity");
            assert_eq!(
                host.ino(),
                linked.ino(),
                "{name}: exported hard-link identity"
            );
            assert_eq!(host.nlink().to_string(), row[3]);
            assert_eq!(std::fs::read(&local).unwrap(), b"linked content\n");
        }
    }
    assert_eq!(
        std::fs::read(&image).unwrap(),
        before,
        "export changed its source"
    );
}

/// Below `/tree`: `file` and `sub` at depth 1, `sub/file` and `sub/a` at 2,
/// `sub/a/b` at 3 and the deepest entry, `sub/a/b/deep`, at [`POLICY_DEPTH`].
const POLICY_STAGE: &str = "mkdir -p tree/sub/a/b\nprintf 'top\\n' > tree/file\n\
     printf 'sub file\\n' > tree/sub/file\npython3 - <<'PY'\n\
     with open('tree/sub/a/b/deep', 'wb') as f:\n    f.write(bytes(range(256)) * 64 + b'!')\nPY\n";
const POLICY_DEPTH: usize = 4;
const POLICY_FILES: [&str; 3] = ["file", "sub/file", "sub/a/b/deep"];

#[test]
fn recursive_export_bounds_merges_replaces_and_honours_the_offset() {
    let dir = ScratchDir::new("cli-export-tree-policy");
    let image = dir.join("source.sqfs");
    build(&image, POLICY_STAGE);
    let before = std::fs::read(&image).unwrap();
    let kernel = guest_kernel_report(image.to_str().unwrap(), "tree export policy reference");
    let assert_files = |root: &Path, names: &[&str]| {
        for name in names {
            let want = kernel
                .get(&("sha256".to_string(), format!("tree/{name}")))
                .unwrap_or_else(|| panic!("the kernel digested no tree/{name}"));
            let bytes = std::fs::read(root.join(name)).unwrap();
            assert_eq!(
                &sha256_hex(&bytes),
                want,
                "{}/{name}: exported versus kernel",
                root.display()
            );
        }
    };

    // Exact fit: the deepest entry sits at --max-depth and is exported.
    let fit = dir.join("fit");
    let depth = POLICY_DEPTH.to_string();
    let out = export_tree(&image, &[], &["--max-depth", &depth], &fit);
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{report}{}", stderr(&out));
    assert_eq!(json_field(&report, "completed"), "7", "{report}");
    assert_eq!(json_field(&report, "failed"), "0", "{report}");
    assert_eq!(json_field(&report, "merged_directories"), "0", "{report}");
    assert_files(&fit, &POLICY_FILES);

    // Overflow: one level less fails the deepest entry and nothing else.
    let over = dir.join("over");
    let shallow = (POLICY_DEPTH - 1).to_string();
    let out = export_tree(&image, &[], &["--max-depth", &shallow], &over);
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
    assert_eq!(json_field(&report, "completed"), "6", "{report}");
    assert_eq!(json_field(&report, "failed"), "1", "{report}");
    let deep = item(&report, "/tree/sub/a/b/deep");
    assert!(deep.contains("\"status\": \"failed\""), "{deep}");
    assert!(deep.contains("maximum depth exceeded"), "{deep}");
    assert!(over.join("sub/a/b").is_dir());
    assert!(std::fs::symlink_metadata(over.join("sub/a/b/deep")).is_err());
    assert_files(&over, &POLICY_FILES[..2]);

    // Existing directories are merged into; their other entries survive.
    let merged = dir.join("merged");
    std::fs::create_dir_all(merged.join("sub/a")).unwrap();
    std::fs::write(merged.join("sub/local-only"), b"not from the image").unwrap();
    let out = export_tree(&image, &[], &[], &merged);
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{report}{}", stderr(&out));
    assert_eq!(json_field(&report, "completed"), "7", "{report}");
    assert_eq!(json_field(&report, "merged_directories"), "3", "{report}");
    for path in ["/tree", "/tree/sub", "/tree/sub/a"] {
        let entry = item(&report, path);
        assert!(entry.contains("merged into existing directory"), "{entry}");
    }
    assert!(!item(&report, "/tree/sub/a/b").contains("merged into existing directory"));
    assert_eq!(
        std::fs::read(merged.join("sub/local-only")).unwrap(),
        b"not from the image"
    );
    assert_files(&merged, &POLICY_FILES);

    // Existing names are refused, never replaced, without --replace; with
    // it only an existing file is replaced, never a directory or a file
    // standing where a directory belongs.
    let kept = dir.join("kept");
    std::fs::create_dir_all(kept.join("file")).unwrap();
    std::fs::create_dir_all(kept.join("sub")).unwrap();
    std::fs::write(kept.join("sub/file"), b"keep me").unwrap();
    std::fs::write(kept.join("sub/a"), b"not a directory").unwrap();
    for replace in [false, true] {
        let flags: &[&str] = if replace { &["--replace"] } else { &[] };
        let out = export_tree(&image, &[], flags, &kept);
        let report = stdout(&out);
        assert_eq!(out.status.code(), Some(1), "{report}{}", stderr(&out));
        let refused = if replace { "2" } else { "3" };
        assert_eq!(json_field(&report, "refused"), refused, "{report}");
        assert_eq!(json_field(&report, "failed"), "0", "{report}");
        let file = item(&report, "/tree/file");
        assert!(file.contains("\"status\": \"refused\""), "{file}");
        assert!(
            file.contains("--replace permits existing files only"),
            "{file}"
        );
        assert!(kept.join("file").is_dir());
        let a = item(&report, "/tree/sub/a");
        assert!(a.contains("\"status\": \"refused\""), "{a}");
        assert!(
            a.contains("destination exists and is not a directory"),
            "{a}"
        );
        assert!(!report.contains("\"path\": \"/tree/sub/a/b\""), "{report}");
        assert_eq!(
            std::fs::read(kept.join("sub/a")).unwrap(),
            b"not a directory"
        );
        let sub_file = item(&report, "/tree/sub/file");
        if replace {
            assert!(sub_file.contains("\"status\": \"completed\""), "{sub_file}");
            assert_files(&kept, &["sub/file"]);
        } else {
            assert!(sub_file.contains("\"status\": \"refused\""), "{sub_file}");
            assert_eq!(std::fs::read(kept.join("sub/file")).unwrap(), b"keep me");
        }
    }

    // An image at an offset exports the same tree only through --offset.
    let embedded = dir.join("embedded");
    let mut shifted_bytes = vec![0; 4096];
    shifted_bytes.extend_from_slice(&before);
    std::fs::write(&embedded, shifted_bytes).unwrap();
    let unshifted = dir.join("unshifted");
    let out = export_tree(&embedded, &[], &[], &unshifted);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(!unshifted.exists());
    let shifted = dir.join("shifted");
    let out = export_tree(&embedded, &["--offset", "4096"], &[], &shifted);
    let report = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{report}{}", stderr(&out));
    assert_eq!(json_field(&report, "completed"), "7", "{report}");
    assert_files(&shifted, &POLICY_FILES);
    assert_eq!(
        std::fs::read(&image).unwrap(),
        before,
        "export changed its source"
    );
}
