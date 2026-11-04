#!/usr/bin/env bash
# 回归：MBR 离线缩容的判据必须在首写之前成立。
#
# 与 GPT 版同一窗口与注入手法（假 dumpe2fs 在 fs_min_bytes 探测期间改表并记账），
# 区别在旧实现不会报错：resize_mdos_entry 只按新尺寸写条目，不比对决策期读到的
# 起点/长度，所以旧实现会先缩 FS、再按已改写的表写条目并以 0 退出。新实现必须在
# shrink_fs 之前拒绝（10），且镜像与改动后的盘面逐字节一致。
source "$(dirname "$0")/lib.sh"
require_fake_bin

T=/var/tmp/recheck-mbr.img
HASH=$T.hash-after-tamper
track_file "$T"

FAKE=$FAKE_TOOL_DIR
cleanup_hook() { rm -rf "$FAKE" 2>/dev/null; rm -f "$HASH"; }

rm -rf "$FAKE"; mkdir -p "$FAKE"
cat > "$FAKE/dumpe2fs" <<EOF
#!/bin/sh
if [ ! -e "$HASH" ]; then
    printf ',450M\n' | sfdisk --force --no-reread -N 1 "$T" >/dev/null 2>&1
    sha256sum "$T" > "$HASH"
fi
echo "Block count:              76800"
echo "Block size:               4096"
EOF
chmod +x "$FAKE/dumpe2fs"

rm -f "$T" "$T"$SIDECAR_GLOB
truncate -s 512M "$T"
$B new "$T" --table msdos --yes >/dev/null
$B create "$T" --size 400M --fs ext4 --yes >/dev/null 2>&1

OUT=$($BFK resize "$T":1 300M --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 10 "首写之前读当前表、不一致即拒绝"
if [ ! -s "$HASH" ]; then
    echo "  BAD  注入未触发：fs_min_bytes 探测期间的表改动没有发生"
    rc=1
fi
assert_eq "$(sha256sum "$T" | cut -d' ' -f1)" "$(cut -d' ' -f1 "$HASH")" \
    "拒绝时盘面逐字节不变（整镜像 sha256）"

exit $rc
