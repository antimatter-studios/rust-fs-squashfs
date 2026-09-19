#!/usr/bin/env bash
# Rebuild fuzz/corpus from images mksquashfs wrote.
#
# One image per compressor, because the compressor id in the superblock
# selects an entirely different decode path and nothing else in the
# format changes with it. All six this crate claims to decode are built,
# so a codec that stopped working is a failing corpus rather than a
# quietly untested one.
#
# The seeds are real images and real structures cut out of them. Random
# bytes are refused by the magic number on the first line of every one
# of these decoders and never reach the arithmetic underneath.
#
# Usage: [MKSQUASHFS=/path/to/mksquashfs] scripts/make-fuzz-corpus.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mksquashfs="${MKSQUASHFS:-mksquashfs}"
work="$(mktemp -d "${TMPDIR:-/tmp}/squashfs-fuzz-corpus.XXXXXX")"
trap 'rm -rf "$work"' EXIT

command -v "$mksquashfs" >/dev/null 2>&1 || [ -x "$mksquashfs" ] || {
    echo "mksquashfs not found; set MKSQUASHFS" >&2
    exit 1
}
command -v setfattr >/dev/null || {
    echo "setfattr not found; install attr (Debian: apt-get install attr)" >&2
    exit 1
}

# Varied enough that each image has something of every shape in it: a
# file large enough to span blocks and use a fragment, one compressible
# and one not, a symlink, a subdirectory, and extended attributes.
tree="$work/tree"
mkdir -p "$tree/sub"
head -c 40000 /dev/urandom > "$tree/random.bin"
python3 -c "import sys; open(sys.argv[1],'w').write('the quick brown fox jumps over the lazy dog. ' * 900)" "$tree/text.txt"
python3 -c "import sys; open(sys.argv[1],'w').write('x' * 5000)" "$tree/runs.txt"
echo "deep" > "$tree/sub/deep.txt"
ln -sf sub/deep.txt "$tree/link"

# Enough entries that the directory table is more than one metablock.
# A handful of files leaves it nearly empty, and the directory decoder
# is then seeded with almost nothing.
for i in $(seq 1 400); do
    : > "$tree/entry-$(printf '%03d' "$i")"
done

setfattr -n user.colour -v blue "$tree/text.txt"
setfattr -n user.a-rather-longer-attribute-name -v "$(printf 'v%.0s' $(seq 1 120))" "$tree/text.txt"
setfattr -n user.x -v y "$tree/sub/deep.txt"

rm -rf "$here/fuzz/corpus"
mkdir -p "$here/fuzz/corpus"/{image,superblock,dir_listing,decompress}

for comp in gzip lzma lzo lz4 xz zstd; do
    img="$here/fuzz/corpus/image/$comp.img"
    # -noappend so a rerun rebuilds rather than adding to the last one;
    # -all-time and -mkfs-time fixed so the corpus is byte-identical
    # across runs and a rebuild is not a diff.
    "$mksquashfs" "$tree" "$img" -comp "$comp" -noappend -no-progress \
        -all-time 0 -mkfs-time 0 -no-exports >/dev/null 2>&1 || {
        echo "mksquashfs could not build the '$comp' image" >&2
        exit 1
    }
done

python3 - "$here/fuzz/corpus" <<'PY'
import os, struct, sys, zlib, lzma

root = sys.argv[1]
MAGIC = 0x73717368  # 'hsqs', little-endian

# Enough of the 96-byte superblock to find the tables.
SB_LEN = 96

def cut(kind, name, data):
    with open(os.path.join(root, kind, name), 'wb') as f:
        f.write(data)

# Python can undo two of the six codecs, which is all this needs: the
# directory decoder does not care which compressor produced the bytes it
# is handed, only that they are a real directory listing.
inflate = {
    'gzip': lambda b: zlib.decompress(b),
    'xz':   lambda b: lzma.decompress(b, format=lzma.FORMAT_XZ),
}

listings = 0
payloads = 0
for img_name in sorted(os.listdir(os.path.join(root, 'image'))):
    comp = img_name[:-len('.img')]
    img = open(os.path.join(root, 'image', img_name), 'rb').read()

    magic, = struct.unpack_from('<I', img, 0)
    assert magic == MAGIC, f"{img_name}: superblock magic is {magic:#x}"
    cut('superblock', f'{comp}.bin', img[:SB_LEN])

    inode_table, = struct.unpack_from('<Q', img, 64)
    directory_table, = struct.unpack_from('<Q', img, 72)
    fragment_table, = struct.unpack_from('<Q', img, 80)

    # Metablocks are a 16-bit header then that many bytes: the top bit
    # says the payload was stored rather than compressed, the low 15
    # give its length. Walking the directory table this way needs no
    # knowledge of what is inside the blocks.
    at = directory_table
    end = fragment_table if fragment_table > directory_table else len(img)
    index = 0
    while at + 2 <= end:
        header, = struct.unpack_from('<H', img, at)
        size = header & 0x7FFF
        stored = bool(header & 0x8000)
        if size == 0 or at + 2 + size > end:
            break
        payload = img[at + 2:at + 2 + size]
        at += 2 + size

        if not stored:
            # A real compressed metablock, for the decompress target.
            # The compressor is in the file name so the harness knows
            # which one is supposed to decode it.
            cut('decompress', f'{comp}-meta{index}.bin', payload)
            payloads += 1

        # The decompressed listing, for the directory decoder. Only the
        # two codecs Python has; the bytes are the same shape whichever
        # compressor produced them.
        raw = payload if stored else None
        if raw is None and comp in inflate:
            try:
                raw = inflate[comp](payload)
            except Exception:
                raw = None
        if raw:
            cut('dir_listing', f'{comp}-block{index}.bin', raw)
            listings += 1
        index += 1

    assert index, f"{img_name}: no metablocks found in the directory table"

assert listings, "no directory listing could be decompressed -- check the gzip and xz images"
assert payloads, "no compressed metablock found -- did every block store uncompressed?"
PY

echo "corpus rebuilt under fuzz/corpus:"
find "$here/fuzz/corpus" -type f | sort | sed "s#$here/##" | head -40
echo "total: $(find "$here/fuzz/corpus" -type f | wc -l) seeds, $(du -sh "$here/fuzz/corpus" | cut -f1)"
