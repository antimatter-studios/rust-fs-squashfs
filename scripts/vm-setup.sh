#!/usr/bin/env bash
#
# vm-setup.sh — the fs-linux-test-harness [setup] script. Runs as root
# INSIDE the VM, re-applied by the harness whenever this file changes.
#
# THE GUEST IS WHERE THE ORACLE TOOLS LIVE. Not the host. squashfs-tools
# on a workstation is whatever that machine has, and this suite already
# has a failing test to prove that matters:
#
#   * Debian 12 packages 4.5.1, which does not know `-xattrs-add`. On such
#     a host `the_trusted_and_security_namespaces_are_assembled_correctly`
#     fails with `mksquashfs: invalid option` while passing on CI, whose
#     runner archive is newer. A test whose verdict depends on the machine
#     is not a test of this driver.
#   * mksquashfs compiles each compressor in ONLY when its library was
#     present at build time. This crate claims gzip, lzo, lz4, xz, zstd
#     and lzma, and a codec claimed as supported and never exercised
#     against a real image is the gap #42 was opened for — so every one of
#     them is asked to produce an image below rather than trusted.
#   * On a Mac there is no SquashFS at all, packaged or otherwise.
#
# So it is built here, from source, at a pinned tag, with every codec: one
# version, one platform, the same answers for everyone, and a developer's
# machine installs none of it.
#
#   squashfs-tools  mksquashfs, unsquashfs, sqfstar — the oracle tools and
#                   the fixture builder's formatter
#   attr, acl       setfattr/getfattr, setfacl/getfacl: the xattrs the
#                   oracle tests read back
#   util-linux      losetup and mount: the kernel oracle's loop mounts,
#                   which happen here and nowhere else
#   squashfs.ko     the real in-kernel SquashFS driver, which is the only
#                   second implementation of the MOUNT path that exists
#
# AND A RUST TOOLCHAIN, for `chore test:vm` — the whole suite compiled and
# run in here, which is how a macOS host runs a Linux test suite at all.
# It is pinned to the repository's rust-toolchain.toml, installed under
# /var/lib (the VM's own disk, which outlives a `vm:down`), and the build
# directory lives there too so the second run is incremental.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

REPO=/repo
RUST_ROOT=/var/lib/fs-squashfs-rust
export RUSTUP_HOME="$RUST_ROOT/rustup"
export CARGO_HOME="$RUST_ROOT/cargo"

# THE squashfs-tools PIN. A tag, not a branch: the family pins every
# sibling, every box and every toolchain, and an oracle that moves on its
# own is an oracle whose verdict cannot be compared with yesterday's.
#
# 4.6.1 rather than the newest (4.7.5 at the time of writing) ON PURPOSE.
# 4.6 is the first series with `-xattrs-add`, which this suite needs, and
# 4.6.1 is what the CI runner's archive has been providing — so it is the
# version every currently-passing assertion was actually validated
# against. Moving to 4.7.x is a deliberate change: the tools' output
# wording is what several tests read, so expect to re-check them.
SQUASHFS_TOOLS_PIN=4.6.1
SQUASHFS_TOOLS_REPO=https://github.com/plougher/squashfs-tools.git
SQUASHFS_TOOLS_SRC=/var/lib/fs-squashfs-tools
SQUASHFS_TOOLS_PREFIX=/usr/local

apt-get update -qq
# liblzo2-dev is here as a BUILD INPUT FOR AN ORACLE, not a dependency of
# anything this project ships: mksquashfs is spawned as a separate
# process, and nothing is linked or copied. The project's licence rule
# draws exactly that line.
apt-get install -y -qq attr acl util-linux curl gcc libc6-dev pkg-config \
    python3 \
    git make \
    zlib1g-dev liblzma-dev liblzo2-dev liblz4-dev libzstd-dev >/dev/null

# The loop driver for the kernel oracle's mounts, and the SquashFS module
# itself. Loaded here rather than at first use so a guest image without
# either fails the provision -- which names this script -- instead of
# failing one test with a mount error nobody can place.
modprobe loop
modprobe squashfs
grep -qw squashfs /proc/filesystems || {
    echo "vm-setup: this guest kernel has no SquashFS driver, so the kernel oracle" >&2
    echo "          cannot mount anything. $(uname -r)" >&2
    exit 1
}

# squashfs-tools, at the pinned tag. Idempotent by the stamp, which holds
# the tag that was built: a changed pin rebuilds, an unchanged one costs a
# `cat`. The harness re-runs this whole script whenever it changes (it
# stamps the script's own sha256 in the guest), so bumping the pin above
# is all it takes for the next boot to rebuild.
stamp="$SQUASHFS_TOOLS_PREFIX/lib/squashfs-tools.pin"
if [ "$(cat "$stamp" 2>/dev/null || true)" != "$SQUASHFS_TOOLS_PIN" ]; then
    echo "vm-setup: building squashfs-tools $SQUASHFS_TOOLS_PIN"
    if [ ! -d "$SQUASHFS_TOOLS_SRC/.git" ]; then
        rm -rf "$SQUASHFS_TOOLS_SRC"
        git init --quiet "$SQUASHFS_TOOLS_SRC"
        git -C "$SQUASHFS_TOOLS_SRC" remote add origin "$SQUASHFS_TOOLS_REPO"
    fi
    git -C "$SQUASHFS_TOOLS_SRC" fetch --quiet --depth 1 origin "$SQUASHFS_TOOLS_PIN"
    git -C "$SQUASHFS_TOOLS_SRC" checkout --quiet FETCH_HEAD

    # EVERY CODEC ASKED FOR EXPLICITLY. Each is compiled in only when its
    # *_SUPPORT flag is set, and a missing one is not an error -- the tool
    # simply refuses that `-comp` later, which would make the oracle test
    # for it compare this crate against nothing. The build is checked
    # below rather than trusted.
    #
    # `make install` is not used: its target and variable names have moved
    # between releases, and installing three known binaries by hand cannot
    # silently install nothing.
    make -C "$SQUASHFS_TOOLS_SRC/squashfs-tools" -j"$(nproc)" \
        GZIP_SUPPORT=1 \
        XZ_SUPPORT=1 \
        LZO_SUPPORT=1 \
        LZ4_SUPPORT=1 \
        ZSTD_SUPPORT=1 \
        LZMA_XZ_SUPPORT=1 \
        XATTR_SUPPORT=1 \
        >/dev/null
    install -d "$SQUASHFS_TOOLS_PREFIX/bin" "$SQUASHFS_TOOLS_PREFIX/lib"
    for tool in mksquashfs unsquashfs sqfstar; do
        install -m 0755 "$SQUASHFS_TOOLS_SRC/squashfs-tools/$tool" \
            "$SQUASHFS_TOOLS_PREFIX/bin/$tool"
    done
    # LAST, so an interrupted build is not mistaken for a finished one.
    printf '%s\n' "$SQUASHFS_TOOLS_PIN" > "$stamp"
fi

# WHAT WAS BUILT, CHECKED — not what was asked for.
#
# THE VERSION IS AN EXACT MATCH, not a floor, and that is deliberate here:
# the point of building it is that every machine answers the same, so
# "new enough" is the wrong question. It also catches the failure a floor
# would miss — a PACKAGED mksquashfs earlier on PATH. Debian's answers
# `mksquashfs version 4.5.1 (2022/03/17)`, which is the exact string that
# made the xattr test fail on a workstation while CI stayed green.
#
# sed, not head, on the tool: head exits after its count, the tool gets
# SIGPIPE writing the next line, and pipefail turns that into a failed
# setup.
version="$(mksquashfs -version 2>&1 | sed -n 's/^mksquashfs version \([0-9][0-9.]*\).*/\1/p' | sed -n 1p)"
if [ "$version" != "$SQUASHFS_TOOLS_PIN" ]; then
    echo "vm-setup: the mksquashfs on PATH is not the one built here." >&2
    echo "          reports:     ${version:-(no version line)}" >&2
    echo "          expected:    $SQUASHFS_TOOLS_PIN" >&2
    echo "          resolved to: $(command -v mksquashfs || echo 'not on PATH')" >&2
    echo "          A packaged squashfs-tools earlier on PATH is the likely cause;" >&2
    echo "          Debian 12's 4.5.1 does not know -xattrs-add, which is the" >&2
    echo "          difference that fails one xattr test on a workstation and not" >&2
    echo "          on CI." >&2
    exit 1
fi

# EVERY CODEC PRODUCES AN IMAGE, because that is what the tests need it to
# do. A codec the build lacks fails the provision here, naming itself,
# rather than failing one oracle test with a refusal nobody can place.
probe="$(mktemp -d)"
mkdir -p "$probe/src"
# Compressible enough that every codec has something to do. NOT
# `yes ... | head -N`: head closes the pipe, `yes` dies of SIGPIPE, and
# pipefail turns that 141 into a failed provision with no message.
for i in 1 2 3 4 5 6 7 8; do
    # shellcheck disable=SC2046  # the word splitting is the repeat count
    printf 'the same line over and over\n%.0s' $(seq 400) > "$probe/src/f$i.txt"
done
for codec in gzip lzo lz4 xz zstd lzma; do
    rm -f "$probe/out.img"
    if ! mksquashfs "$probe/src" "$probe/out.img" -comp "$codec" -no-progress \
        >/dev/null 2>"$probe/err"; then
        echo "vm-setup: this mksquashfs cannot write $codec:" >&2
        sed -n '1,5p' "$probe/err" >&2
        echo "          It was built without that codec's library, and the oracle" >&2
        echo "          tests for it would compare this crate against nothing." >&2
        exit 1
    fi
    # unsquashfs has its own decoders, and a codec mksquashfs can write
    # but unsquashfs cannot read is still a broken oracle pair.
    if ! unsquashfs -stat "$probe/out.img" >/dev/null 2>"$probe/err"; then
        echo "vm-setup: unsquashfs cannot read the $codec image mksquashfs just wrote:" >&2
        sed -n '1,5p' "$probe/err" >&2
        exit 1
    fi
done
rm -rf "$probe"

echo "vm-setup: mksquashfs $version writes gzip, lzo, lz4, xz, zstd and lzma"
unsquashfs -version 2>&1 | sed -n 1p

# The toolchain the repository pins, and only that one: a guest that
# silently built with a different compiler than CI is a guest whose
# result means nothing.
toolchain="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' "$REPO/rust-toolchain.toml" | sed -n 1p)"
[ -n "$toolchain" ] || { echo "vm-setup: no channel in $REPO/rust-toolchain.toml" >&2; exit 1; }

mkdir -p "$RUST_ROOT"
if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --no-modify-path --default-toolchain none >/dev/null
fi
"$CARGO_HOME/bin/rustup" toolchain install "$toolchain" \
    --component rustfmt --component clippy --profile minimal >/dev/null
"$CARGO_HOME/bin/rustup" default "$toolchain" >/dev/null
"$CARGO_HOME/bin/cargo" --version

echo "vm-setup: the oracle tools, the SquashFS driver and the pinned toolchain are in the guest"
