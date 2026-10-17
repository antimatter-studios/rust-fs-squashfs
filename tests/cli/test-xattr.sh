# Empty sets, refusals and generated documentation on the installed tool.
# Populated sets and byte values are checked against Linux in cli_xattr_kernel.rs.
source "$(dirname "$0")/lib.sh"

img="$REPO/test-disks/squashfs-basic.sqfs"
cp "$img" "$SANDBOX/before.sqfs"
for node in / /hello.txt /sub /link; do
    fs.squashfs "$img" xattr "$node" >"$SANDBOX/list" 2>"$SANDBOX/err"
    check "xattr $node exits 0" test $? -eq 0
    jq_check "xattr $node has no attributes" '.==[]' "$SANDBOX/list"
    check "xattr $node has no error" test ! -s "$SANDBOX/err"
done
fs.squashfs "$img" xattr --text /hello.txt >"$SANDBOX/text" 2>"$SANDBOX/err"
check 'empty --text exits 0' test $? -eq 0
check 'empty --text is silent' test ! -s "$SANDBOX/text"

for args in '/hello.txt user.missing' '/missing' '/missing user.missing'; do
    # shellcheck disable=SC2086 # Each case is a fixed sequence of operands.
    fs.squashfs "$img" xattr $args >"$SANDBOX/out" 2>"$SANDBOX/err"
    check "xattr $args exits 1" test $? -eq 1
    check "xattr $args has no stdout" test ! -s "$SANDBOX/out"
    jq_check "xattr $args has a structured error" '.code==1 and (.error|type)=="string"' "$SANDBOX/err"
done
fs.squashfs "$img" xattr /hello.txt user.missing -o "$SANDBOX/value" >"$SANDBOX/out" 2>"$SANDBOX/err"
check 'missing attribute exits 1 with -o' test $? -eq 1
check 'missing attribute names itself' grep -q 'user.missing' "$SANDBOX/err"
check 'missing attribute creates no output file' test ! -e "$SANDBOX/value"
check 'missing attribute creates no partial file' test ! -e "$SANDBOX/value.partial"

for args in '' '/hello.txt -o value' '/hello.txt user.a extra'; do
    # shellcheck disable=SC2086
    fs.squashfs "$img" xattr $args >"$SANDBOX/out" 2>"$SANDBOX/err"
    check "invalid xattr $args exits 2" test $? -eq 2
    check 'usage error has no stdout' test ! -s "$SANDBOX/out"
    jq_check 'usage error is structured' '.code==2' "$SANDBOX/err"
done
fs.squashfs "$img" xattr --help >"$SANDBOX/help" 2>/dev/null
check 'xattr help exits 0' test $? -eq 0
check 'xattr help has examples' grep -q 'Examples:' "$SANDBOX/help"
check 'xattr leaves image unchanged' cmp -s "$img" "$SANDBOX/before.sqfs"

finish
