# fs.squashfs on the committed test-disks/squashfs-basic.sqfs (its contents
# are in squashfs-basic.meta.txt): ls, read, get/info with the canonical
# keys and their types, the verbs a read-only format refuses, --offset, and
# damaged images refused with a structured error and nothing on stdout.
source "$(dirname "$0")/lib.sh"

img="$REPO/test-disks/squashfs-basic.sqfs"
sha256() { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi | cut -d' ' -f1; }
check "the committed fixture is present" test -s "$img"

# ls: the root, every field typed, and a symlink's target.
fs.squashfs "$img" ls / >"$SANDBOX/ls.json" 2>"$SANDBOX/ls.err"
check "ls / exits 0 ($(cat "$SANDBOX/ls.err"))" test $? -eq 0
jq_check "ls / lists the four root entries" \
    '[.[].name] == ["empty.txt", "hello.txt", "link", "sub"]' "$SANDBOX/ls.json"
jq_check "every entry is typed" \
    'all(.[]; (.name|type)=="string" and (.type|type)=="string" and (.size|type)=="number" and (.mode|test("^[0-7]{4}$")) and (.mtime|type)=="number" and (.inode|type)=="number")' \
    "$SANDBOX/ls.json"
jq_check "the types are file, file, symlink, dir" \
    '[.[].type] == ["file", "file", "symlink", "dir"]' "$SANDBOX/ls.json"
jq_check "the symlink's target is hello.txt, and only it has one" \
    '[.[] | select(has("target"))] == [.[] | select(.name == "link")] and (.[] | select(.name == "link") | .target) == "hello.txt"' \
    "$SANDBOX/ls.json"
fs.squashfs "$img" ls /sub/deep >"$SANDBOX/deep.json" 2>/dev/null
jq_check "ls /sub/deep is big.bin at 20000 bytes" \
    '. | length == 1 and .[0].name == "big.bin" and .[0].size == 20000' "$SANDBOX/deep.json"
fs.squashfs "$img" ls --text / >"$SANDBOX/ls.txt" 2>/dev/null
check "ls --text / is one line per entry" test "$(wc -l <"$SANDBOX/ls.txt" | tr -d ' ')" = 4
check "ls --text shows the symlink's target" grep -q ' link -> hello.txt$' "$SANDBOX/ls.txt"

# read: exact bytes, to stdout and to a file.
check "read /hello.txt is hi" test "$(fs.squashfs "$img" read /hello.txt | od -An -c | tr -d ' ')" = 'hi\n'
got="$(fs.squashfs "$img" read /sub/deep/big.bin | sha256)"
check "read /sub/deep/big.bin is the fixture's 20000 bytes ($got)" \
    test "$got" = 29d3a07eeca8adc1e740d5215c12e9146b383b3fc176e9b24458550a7dc59d03
got="$(fs.squashfs "$img" read /sub/note.md | sha256)"
check "read /sub/note.md is the fixture's note" \
    test "$got" = bb281e9297a5a79136d0beb4cc0f65047bb996db110dc66c01db43b906ef8a08
fs.squashfs "$img" read /empty.txt >"$SANDBOX/empty.out" 2>/dev/null
check "read /empty.txt exits 0 with nothing" test $? -eq 0 -a ! -s "$SANDBOX/empty.out"
fs.squashfs "$img" read /sub/deep/big.bin -o "$SANDBOX/big.bin" >"$SANDBOX/o.out" 2>/dev/null
check "read -o exits 0 and prints nothing" test $? -eq 0 -a ! -s "$SANDBOX/o.out"
check "read -o wrote the file" test "$(wc -c <"$SANDBOX/big.bin" | tr -d ' ')" = 20000

# get / info: every canonical key, typed; get and info identical.
fs.squashfs "$img" get >"$SANDBOX/get.json" 2>/dev/null
check "get exits 0" test $? -eq 0
jq_check "get carries every canonical key with its type" \
    '(.fs=="squashfs") and .label==null and (.total_bytes|type)=="number" and .free_bytes==0 and (.block_size|type)=="number" and .dirty==false and (.squashfs|type)=="object"' \
    "$SANDBOX/get.json"
jq_check "the fixture is gzip at 4096-byte blocks, version 4.0" \
    '.block_size==4096 and .squashfs.compression=="gzip" and .squashfs.version=="4.0"' "$SANDBOX/get.json"
jq_check "total_bytes is the image's bytes used" '.total_bytes == .squashfs.bytes_used' "$SANDBOX/get.json"
fs.squashfs "$img" info >"$SANDBOX/info.json" 2>/dev/null
check "info and get print the same" cmp -s "$SANDBOX/get.json" "$SANDBOX/info.json"
fs.squashfs "$img" get label >"$SANDBOX/label.json" 2>/dev/null
jq_check "get label is null: the format has none" '. == {"label": null}' "$SANDBOX/label.json"
check "get squashfs.compression --text is gzip" \
    test "$(fs.squashfs "$img" get squashfs.compression --text)" = gzip

# The verbs a read-only format refuses: status 3, nothing on stdout.
for verb in "write /new" "mkdir /newdir" "set label X" "resize 1G --force"; do
    # shellcheck disable=SC2086  # the words are the point
    fs.squashfs "$img" $verb </dev/null >"$SANDBOX/ro.out" 2>"$SANDBOX/ro.err"
    check "$verb exits 3" test $? -eq 3
    check "$verb prints nothing on stdout" test ! -s "$SANDBOX/ro.out"
    jq_check "$verb says SquashFS is read-only" '.code == 3 and (.error | test("SquashFS is read-only"))' "$SANDBOX/ro.err"
done

# --offset: the image embedded a MiB into a larger file.
{ head -c 1048576 /dev/zero; cat "$img"; } >"$SANDBOX/embedded.img"
check "--offset reaches the embedded image" \
    test "$(fs.squashfs --offset 1048576 "$SANDBOX/embedded.img" read /hello.txt | od -An -c | tr -d ' ')" = 'hi\n'
fs.squashfs "$SANDBOX/embedded.img" ls >"$SANDBOX/noff.out" 2>"$SANDBOX/noff.err"
check "without --offset the embedded image is refused" test $? -eq 1 -a ! -s "$SANDBOX/noff.out"

# Failures: status 1, a structured error, nothing on stdout -- never a panic.
refuse() {
    local what="$1"
    shift
    "$@" >"$SANDBOX/f.out" 2>"$SANDBOX/f.err"
    check "$what exits 1 (not a panic)" test $? -eq 1
    check "$what prints nothing on stdout" test ! -s "$SANDBOX/f.out"
    jq_check "$what is a structured error" '.code == 1 and (.error|type) == "string"' "$SANDBOX/f.err"
}
refuse "read of a directory" fs.squashfs "$img" read /sub
refuse "read of a symlink" fs.squashfs "$img" read /link
refuse "ls of a missing path" fs.squashfs "$img" ls /missing
refuse "a missing image" fs.squashfs "$SANDBOX/absent.sqfs" ls /
cp "$img" "$SANDBOX/magic.sqfs"
printf 'XXXX' | dd of="$SANDBOX/magic.sqfs" bs=1 seek=0 conv=notrunc 2>/dev/null
refuse "a bad superblock magic" fs.squashfs "$SANDBOX/magic.sqfs" get
head -c 4096 "$img" >"$SANDBOX/cut.sqfs"
refuse "a truncated image" fs.squashfs "$SANDBOX/cut.sqfs" ls /
refuse "a file read from a truncated image" fs.squashfs "$SANDBOX/cut.sqfs" read /sub/deep/big.bin
# The inode table's first compressed metadata block, its deflate stream
# overwritten: every lookup has to decompress it.
table="$(od -An -t u8 -j 64 -N 8 "$img" | tr -d ' ')"
cp "$img" "$SANDBOX/meta.sqfs"
head -c 16 /dev/zero | tr '\0' '\377' | dd of="$SANDBOX/meta.sqfs" bs=1 seek=$(( table + 4 )) conv=notrunc 2>/dev/null
refuse "a damaged metadata block" fs.squashfs "$SANDBOX/meta.sqfs" ls /
refuse "a file behind a damaged metadata block" fs.squashfs "$SANDBOX/meta.sqfs" read /hello.txt

finish
