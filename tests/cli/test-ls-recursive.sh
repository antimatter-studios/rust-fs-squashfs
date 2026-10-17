# Bounded, deterministic depth-first listing on the committed reference image.
# The mounted reference tree and malformed directory graphs are checked in
# cli_ls_recursive_kernel.rs through the guest support helpers.
source "$(dirname "$0")/lib.sh"

img="$REPO/test-disks/squashfs-basic.sqfs"
cp "$img" "$SANDBOX/before.sqfs"
fs.squashfs "$img" ls --recursive / >"$SANDBOX/tree.json" 2>"$SANDBOX/tree.err"
check "recursive ls exits 0" test $? -eq 0
check "successful recursion has no error" test ! -s "$SANDBOX/tree.err"
jq_check "paths follow sorted depth-first order" \
    '[.[].path]==["/empty.txt","/hello.txt","/link","/sub","/sub/deep","/sub/deep/big.bin","/sub/note.md"]' "$SANDBOX/tree.json"
jq_check "depth counts edges from the requested directory" \
    '[.[].depth]==[1,1,1,1,2,3,2]' "$SANDBOX/tree.json"
jq_check "every recursive entry retains the ls shape" \
    'all(.[]; (.name|type)=="string" and (.type|type)=="string" and (.size|type)=="number" and (.mode|test("^[0-7]{4}$")) and (.mtime|type)=="number" and (.inode|type)=="number" and (.path|type)=="string" and (.depth|type)=="number" and (has("revisited")|not))' "$SANDBOX/tree.json"
fs.squashfs "$img" ls -R / >"$SANDBOX/again.json" 2>/dev/null
check "-R exits 0" test $? -eq 0
check "two runs print identical bytes" cmp -s "$SANDBOX/tree.json" "$SANDBOX/again.json"
fs.squashfs "$img" ls -R --max-depth 3 --limit 7 / >"$SANDBOX/fit.json" 2>/dev/null
check "an exact fit succeeds" test $? -eq 0
check "an exact fit retains every entry" cmp -s "$SANDBOX/tree.json" "$SANDBOX/fit.json"

for spec in 'max-depth 2 5 /sub/deep/big.bin' 'limit 4 4 /sub/deep' 'max-depth 0 0 /empty.txt' 'limit 0 0 /empty.txt'; do
    read -r bound value count stopped <<<"$spec"
    fs.squashfs "$img" ls -R "--$bound" "$value" / >"$SANDBOX/partial.json" 2>"$SANDBOX/partial.err"
    check "$bound $value exits 1" test $? -eq 1
    jq_check "$bound $value retains its collected prefix" \
        "length==$count" "$SANDBOX/partial.json"
    check "$bound $value retains exactly the expected entries" \
        test "$(jq -c . "$SANDBOX/partial.json")" = "$(jq -c ".[0:$count]" "$SANDBOX/tree.json")"
    jq_check "$bound $value names the bound and stopped path" \
        ".code==1 and (.error|contains(\"--$bound\")) and (.error|contains(\"$stopped\"))" "$SANDBOX/partial.err"
done

fs.squashfs "$img" ls -R sub//deep/ >"$SANDBOX/sub.json" 2>/dev/null
check "relative and repeated separators resolve" test $? -eq 0
jq_check "subtree paths are volume absolute with relative depth" \
    'length==1 and .[0].path=="/sub/deep/big.bin" and .[0].depth==1' "$SANDBOX/sub.json"
fs.squashfs "$img" ls -R /link >"$SANDBOX/link.json" 2>/dev/null
jq_check "a starting symlink is listed without following it" \
    'length==1 and .[0].path=="/link" and .[0].depth==0 and .[0].type=="symlink" and .[0].target=="hello.txt"' "$SANDBOX/link.json"
fs.squashfs "$img" ls -R --text / >"$SANDBOX/tree.txt" 2>/dev/null
check "text recursion exits 0" test $? -eq 0
check "text recursion uses full paths" grep -q ' /sub/deep/big.bin$' "$SANDBOX/tree.txt"
fs.squashfs --text "$img" ls -R --json --limit 1 / >"$SANDBOX/format.json" 2>"$SANDBOX/format.err"
check "partial output respects the last format switch" test $? -eq 1
jq_check "partial output is JSON when JSON was last" 'length==1' "$SANDBOX/format.json"
jq_check "partial failure retains the shared structured error" '.code==1' "$SANDBOX/format.err"
fs.squashfs "$img" ls / >"$SANDBOX/plain.json" 2>/dev/null
jq_check "plain ls has no recursive fields" \
    'length==4 and all(.[]; (has("path") or has("depth") or has("revisited"))|not)' "$SANDBOX/plain.json"

for args in '--max-depth 2' '--limit 2' '-R --limit nope' '-R --max-depth -1'; do
    # shellcheck disable=SC2086 # The words form the command line under test.
    fs.squashfs "$img" ls $args / >"$SANDBOX/usage.out" 2>"$SANDBOX/usage.err"
    check "ls $args is a usage error" test $? -eq 2
    check "ls $args prints nothing on stdout" test ! -s "$SANDBOX/usage.out"
    jq_check "ls $args has a structured usage error" '.code==2' "$SANDBOX/usage.err"
done
fs.squashfs "$img" ls -R /missing >"$SANDBOX/missing.out" 2>"$SANDBOX/missing.err"
check "missing starting path exits 1 without a report" test $? -eq 1 -a ! -s "$SANDBOX/missing.out"
{ head -c 1048576 /dev/zero; cat "$img"; } >"$SANDBOX/embedded.img"
fs.squashfs --offset 1048576 "$SANDBOX/embedded.img" ls -R / >"$SANDBOX/offset.json" 2>/dev/null
check "recursive --offset reaches the filesystem" test $? -eq 0
check "recursive --offset preserves metadata" cmp -s "$SANDBOX/tree.json" "$SANDBOX/offset.json"
check "all commands leave the image unchanged" cmp -s "$img" "$SANDBOX/before.sqfs"

finish
