//! Shared bounded exports checked against unsquashfs and the Linux kernel.

mod cli_support;

use cli_support::*;
use fs_core::FileDevice;
use fs_squashfs::Filesystem;
use fs_squashfs_test_support::{
    assert_unsquashfs_walks, guest_kernel_report, mksquashfs_from_guest_tree, oracle, sha256_hex,
    ScratchDir,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const STAGE: &str = r#"
touch empty
python3 - <<'PY'
with open('big', 'wb') as f:
    f.write(bytes(range(256)) * 12288 + b'!')
PY
mkdir dir
ln -s big link
mkfifo pipe
"#;

fn image(dir: &ScratchDir, comp: &str) -> PathBuf {
    let image = dir.join("source.sqfs");
    let built = mksquashfs_from_guest_tree(
        &image,
        STAGE,
        &[
            "-comp",
            comp,
            "-b",
            "4096",
            "-no-fragments",
            "-noappend",
            "-no-progress",
        ],
    );
    assert!(built.status.success(), "mksquashfs: {}", stderr(&built));
    assert_unsquashfs_walks(image.to_str().unwrap(), "export source");
    image
}

#[test]
fn export_matches_kernel_and_unsquashfs_with_shared_publication_rules() {
    for comp in ["gzip", "xz", "lz4", "zstd", "lzo"] {
        let dir = ScratchDir::new(&format!("cli-export-{comp}"));
        let image = image(&dir, comp);
        let before = std::fs::read(&image).unwrap();
        let kernel = guest_kernel_report(image.to_str().unwrap(), "export source");
        for (name, size) in [("empty", 0), ("big", 3_145_729)] {
            let want = kernel
                .get(&("sha256".to_string(), name.to_string()))
                .expect("the kernel digested the source file");
            let reference = oracle("unsquashfs")
                .args(["-cat", image.to_str().unwrap(), name])
                .output();
            assert!(reference.status.success(), "{}", stderr(&reference));
            assert_eq!(&sha256_hex(&reference.stdout), want);
            let path = format!("/{name}");
            let dest = dir.join(name);
            let report = stdout(&ok(tool("fs.squashfs")
                .arg(&image)
                .args(["export", &path])
                .arg(&dest)));
            assert_eq!(json_field(&report, "bytes"), size.to_string());
            assert_eq!(json_field(&report, "sha256"), *want);
            assert_eq!(json_field(&report, "replaced"), "false");
            let bytes = std::fs::read(&dest).unwrap();
            assert_eq!(bytes.len(), size);
            assert_eq!(&sha256_hex(&bytes), want);
            let raw = ok(tool("fs.squashfs").arg(&image).args(["export", &path, "-"]));
            assert_eq!(raw.stdout, bytes);
            assert!(raw.stderr.is_empty());
            std::fs::write(&dest, b"keep me").unwrap();
            let refused = tool("fs.squashfs")
                .arg(&image)
                .args(["export", &path])
                .arg(&dest)
                .output()
                .unwrap();
            assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
            assert!(refused.stdout.is_empty());
            assert_eq!(std::fs::read(&dest).unwrap(), b"keep me");
            let replaced = stdout(&ok(tool("fs.squashfs")
                .arg(&image)
                .args(["export", "--replace", &path])
                .arg(&dest)));
            assert_eq!(json_field(&replaced, "replaced"), "true");
            assert_eq!(&sha256_hex(&std::fs::read(&dest).unwrap()), want);
            assert_no_staging(&dest);
        }
        for path in ["/dir", "/link", "/pipe", "/missing"] {
            let dest = dir.join("refused");
            let out = tool("fs.squashfs")
                .arg(&image)
                .args(["export", path])
                .arg(&dest)
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
            assert!(out.stdout.is_empty());
            assert!(!dest.exists());
        }
        for args in [
            &["export", "--replace", "/big", "-"][..],
            &["export", "--recursive", "/dir", "tree"],
        ] {
            let usage = tool("fs.squashfs").arg(&image).args(args).output().unwrap();
            assert_eq!(usage.status.code(), Some(2), "{}", stderr(&usage));
            assert!(usage.stdout.is_empty());
        }
        let help = stdout(&ok(tool("fs.squashfs")
            .arg(&image)
            .args(["export", "--help"])));
        assert!(help.contains("Examples:"), "{help}");
        assert!(help.contains("fs.squashfs rootfs.sqfs export"), "{help}");
        let embedded = dir.join("embedded");
        let mut bytes = vec![0; 4096];
        bytes.extend_from_slice(&before);
        std::fs::write(&embedded, bytes).unwrap();
        let out = ok(tool("fs.squashfs")
            .arg(&embedded)
            .args(["--offset", "4096", "export", "/big", "-"]));
        assert_eq!(
            &sha256_hex(&out.stdout),
            kernel.get(&("sha256".into(), "big".into())).unwrap()
        );
        assert_eq!(
            std::fs::read(&image).unwrap(),
            before,
            "export changed its source"
        );
    }
}

#[test]
fn unreadable_export_never_publishes_or_replaces_and_cleans_staging() {
    let dir = ScratchDir::new("cli-export-damaged");
    let image = image(&dir, "gzip");
    let fs = Filesystem::open(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
    let inode = fs.lookup_path("/big").unwrap();
    // Damage a compressed data block after the first 1 MiB, leaving all
    // inode metadata and one entire transfer chunk readable.
    let index = 257;
    let size = inode.block_sizes[index];
    assert_eq!(size & (1 << 24), 0, "the damaged block must be compressed");
    let at = inode.blocks_start as usize
        + inode.block_sizes[..index]
            .iter()
            .map(|size| (size & 0x00ff_ffff) as usize)
            .sum::<usize>();
    let mut bytes = std::fs::read(&image).unwrap();
    bytes[at..at + (size & 0x00ff_ffff) as usize].fill(0);
    std::fs::write(&image, bytes).unwrap();
    let reference = oracle("unsquashfs")
        .args(["-cat", image.to_str().unwrap(), "big"])
        .output();
    assert!(
        !reference.status.success(),
        "unsquashfs accepted damaged data"
    );
    let damaged = Filesystem::open(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
    let inode = damaged.lookup_path("/big").unwrap();
    let mut chunk = vec![0; 1 << 20];
    assert_eq!(
        damaged.read_file(&inode, 0, &mut chunk).unwrap(),
        chunk.len()
    );
    assert!(damaged.read_file(&inode, 1 << 20, &mut chunk).is_err());
    let dest = dir.join("destination");
    let out = tool("fs.squashfs")
        .arg(&image)
        .args(["export", "/big"])
        .arg(&dest)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert!(!dest.exists());
    assert_no_staging(&dest);
    std::fs::write(&dest, b"original destination").unwrap();
    let out = tool("fs.squashfs")
        .arg(&image)
        .args(["export", "--replace", "/big"])
        .arg(&dest)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert_eq!(std::fs::read(&dest).unwrap(), b"original destination");
    assert_no_staging(&dest);
}

fn assert_no_staging(dest: &Path) {
    let stem = format!(".{}.", dest.file_name().unwrap().to_string_lossy());
    for entry in std::fs::read_dir(dest.parent().unwrap()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.starts_with(&stem), "staging file remained: {name}");
    }
}
