# The shared stat shape and refusals on the committed reference image.
# Values on freshly built images, including devices, are checked against
# the reference tools and Linux in cli_stat_kernel.rs, inside the VM.
source "$(dirname "$0")/lib.sh"

img="$REPO/test-disks/squashfs-basic.sqfs"
cp "$img" "$SANDBOX/before.sqfs"
shared='["path","inode","type","mode","uid","gid","nlink","size","allocated_bytes","atime","atime_nsec","mtime","mtime_nsec","ctime","ctime_nsec","btime","btime_nsec"]'
typed='(.path|type)=="string" and (.inode|type)=="number" and (.type|type)=="string"
    and (.mode|test("^[0-7]{4}$")) and (.uid|type)=="number" and (.gid|type)=="number"
    and (.nlink|type)=="number" and (.size|type)=="number" and (.mtime|type)=="number"
    and all(.allocated_bytes, .atime, .atime_nsec, .mtime_nsec, .ctime, .ctime_nsec, .btime, .btime_nsec; .==null)
    and (.squashfs|type)=="object" and (.squashfs.inode_type|type)=="number"
    and all(.squashfs.fragment_index, .squashfs.fragment_offset, .squashfs.block_count, .squashfs.xattr_index; type=="number" or type=="null")'

for path in / /sub /hello.txt /empty.txt /link; do
    fs.squashfs "$img" stat "$path" >"$SANDBOX/stat.json" 2>"$SANDBOX/stat.err"
    rc=$?
    check "stat $path exits 0 ($(cat "$SANDBOX/stat.err"))" test "$rc" -eq 0
    jq_check "stat $path starts with the shared keys in order" \
        "[keys_unsorted[]][0:17] == $shared" "$SANDBOX/stat.json"
    jq_check "stat $path has every field typed, absent fields null" "$typed" "$SANDBOX/stat.json"
done

fs.squashfs "$img" stat /hello.txt >"$SANDBOX/file.json" 2>/dev/null
jq_check "stat reports the requested three-byte file" \
    '.path=="/hello.txt" and .type=="file" and .size==3 and .nlink==1 and .squashfs.block_count==0 and (.squashfs.fragment_index|type)=="number"' "$SANDBOX/file.json"
fs.squashfs "$img" ls / >"$SANDBOX/ls.json" 2>/dev/null
check "stat and ls agree on the inode and mode" \
    test "$(jq -c '[.inode,.mode]' "$SANDBOX/file.json")" = "$(jq -c '.[]|select(.name=="hello.txt")|[.inode,.mode]' "$SANDBOX/ls.json")"
fs.squashfs "$img" stat /link >"$SANDBOX/link.json" 2>/dev/null
jq_check "stat reports the symlink without following it" \
    '.type=="symlink" and .target=="hello.txt" and .size==9 and .squashfs.fragment_index==null and .squashfs.fragment_offset==null and .squashfs.block_count==null and .squashfs.xattr_index==null' "$SANDBOX/link.json"
fs.squashfs "$img" stat --text /hello.txt >"$SANDBOX/file.txt" 2>/dev/null
check "stat --text exits 0" test $? -eq 0
check "stat --text prints key: value lines" grep -qx 'size: 3' "$SANDBOX/file.txt"
check "stat --text names the type" grep -qx 'type: file' "$SANDBOX/file.txt"

{ head -c 1048576 /dev/zero; cat "$img"; } >"$SANDBOX/embedded.img"
fs.squashfs --offset 1048576 "$SANDBOX/embedded.img" stat /hello.txt >"$SANDBOX/offset.json" 2>/dev/null
check "stat --offset reaches the filesystem" test $? -eq 0
check "stat --offset reports the same metadata" cmp -s "$SANDBOX/file.json" "$SANDBOX/offset.json"

fs.squashfs "$img" stat /missing >"$SANDBOX/m.out" 2>"$SANDBOX/m.err"
check "stat of a missing path exits 1" test $? -eq 1
check "stat of a missing path prints nothing on stdout" test ! -s "$SANDBOX/m.out"
jq_check "a missing path has a structured error" '.code==1 and (.error|test("not found"))' "$SANDBOX/m.err"
fs.squashfs "$img" stat >"$SANDBOX/u.out" 2>"$SANDBOX/u.err"
check "stat without a path exits 2" test $? -eq 2
check "a usage error prints nothing on stdout" test ! -s "$SANDBOX/u.out"
jq_check "a usage error has a structured error" '.code==2 and (.error|type)=="string"' "$SANDBOX/u.err"
check "stat leaves the image unchanged" cmp -s "$img" "$SANDBOX/before.sqfs"

finish
