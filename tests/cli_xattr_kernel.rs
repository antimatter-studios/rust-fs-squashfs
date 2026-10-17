//! Names and every value come from Linux's mounted reference image.
//! Hex transport preserves invalid UTF-8, NUL values and empty values.

mod cli_support;

use cli_support::*;
use fs_squashfs_test_support::{guest_kernel_read, mksquashfs_from_guest_tree, ScratchDir};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

const STAGE: &str = r#"
mkdir dir
touch file same bare
ln -s file link
python3 - <<'PY'
import os
for p in [b'file', b'same', b'dir', b'raw-\xff']:
    if p.startswith(b'raw-'):
        open(p, 'wb').close()
    for name, value in [(b'user.comment', b'hello\x00\xff\n'),
                        (b'user.empty', b''), (b'user.large', bytes(range(256)) * 40),
                        (b'user.raw-\xff\n"', b'raw-name')]:
        os.setxattr(p, name, value)
PY
"#;

const REPORT: &str = r#"
getfattr -d -m - -e hex "$MNT/file" > /dev/null
python3 - "$MNT" <<'PY'
import json, os, sys
for rel in [b'', b'file', b'same', b'bare', b'dir', b'link', b'raw-\xff']:
    p = os.fsencode(sys.argv[1]) + b'/' + rel
    attrs = sorted((os.fsencode(n), os.getxattr(p, n, follow_symlinks=False))
                   for n in os.listxattr(p, follow_symlinks=False))
    assert any(n.startswith(b'trusted.') for n, v in attrs), rel
    assert any(n.startswith(b'security.') for n, v in attrs), rel
    entries = []
    for n, v in attrs:
        entry = {'name': n.decode('utf-8', 'replace')}
        try:
            n.decode('utf-8')
        except UnicodeDecodeError:
            entry['name_hex'] = n.hex()
        entry['size'] = len(v)
        entries.append(entry)
    listing = json.dumps(entries, ensure_ascii=False, indent=2).encode().hex()
    print((b'/' + rel).hex() + '|' + listing + '|' + '|'.join(n.hex() + '=' + v.hex() for n, v in attrs))
PY
"#;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn xattr_names_and_values_match_the_kernel_in_every_namespace() {
    let dir = ScratchDir::new("cli-xattr");
    for layout in ["-xattrs", "-noX"] {
        let image = dir.join(&format!("{layout}.sqfs"));
        let built = mksquashfs_from_guest_tree(
            &image,
            STAGE,
            &[
                "-noappend",
                "-no-progress",
                layout,
                "-xattrs-add",
                "trusted.audit=reference",
                "-xattrs-add",
                "security.selinux=system_u:object_r:bin_t:s0",
            ],
        );
        assert!(built.status.success(), "mksquashfs: {}", stderr(&built));
        let before = std::fs::read(&image).unwrap();
        let report = guest_kernel_read(image.to_str().unwrap(), REPORT);
        assert!(report.status.success(), "kernel: {}", stderr(&report));
        assert_eq!(stdout(&report).lines().count(), 7);
        for line in stdout(&report).lines() {
            let mut fields = line.split('|');
            let path = unhex(fields.next().unwrap());
            let expected = String::from_utf8(unhex(fields.next().unwrap())).unwrap();
            let attrs: Vec<_> = fields
                .map(|f| {
                    let (n, v) = f.split_once('=').unwrap();
                    (unhex(n), unhex(v))
                })
                .collect();
            if path == b"/file" {
                assert_eq!(attrs.len(), 6, "all namespaces, empty, raw and out-of-line");
            }
            let mut text = Vec::new();
            for (name, value) in &attrs {
                let shown = String::from_utf8_lossy(name).into_owned();
                text.push(format!("{}\t{}", value.len(), shown));
                let mut cmd = tool("fs.squashfs");
                cmd.arg(&image)
                    .arg("xattr")
                    .arg(OsStr::from_bytes(&path))
                    .arg(OsStr::from_bytes(name));
                let out = ok(&mut cmd);
                assert_eq!(&out.stdout, value, "{path:?} {name:?}");
                assert!(out.stderr.is_empty());
                let dest = dir.join("value");
                assert!(
                    ok(cmd.args([OsStr::new("--text"), OsStr::new("-o"), dest.as_os_str()]))
                        .stdout
                        .is_empty()
                );
                assert_eq!(std::fs::read(&dest).unwrap(), *value);
                assert!(!dir.join("value.partial").exists());
            }
            let list = ok(tool("fs.squashfs")
                .arg(&image)
                .arg("xattr")
                .arg(OsStr::from_bytes(&path)));
            assert_eq!(stdout(&list).trim_end(), expected);
            let out = ok(tool("fs.squashfs")
                .arg(&image)
                .args(["xattr", "--text"])
                .arg(OsStr::from_bytes(&path)));
            assert_eq!(stdout(&out).trim_end(), text.join("\n"));
        }
        let embedded = dir.join("embedded");
        let mut bytes = vec![0; 4096];
        bytes.extend_from_slice(&before);
        std::fs::write(&embedded, bytes).unwrap();
        let out = ok(tool("fs.squashfs").arg(&embedded).args([
            "--offset",
            "4096",
            "xattr",
            "/file",
            "user.comment",
        ]));
        assert_eq!(out.stdout, b"hello\0\xff\n");
        assert_eq!(std::fs::read(&image).unwrap(), before);
    }
}
