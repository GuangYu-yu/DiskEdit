#!/usr/bin/env bash
# 回归：MBR 离线缩容的判据必须在首写之前成立。
#
# 窗口取在 fs_min_bytes 探测期间（锁下几何已解析、缩容尚未启动）：假 dumpe2fs 先把
# 盘上目标条目改成别的尺寸，再把改动后的整镜像摘要记下。若拿旧几何直接缩 FS、再写表，
# 镜像已被改写；正确行为是在任何目标写入之前拒绝（5：拒绝前已落恢复记录，盘面与
# 改动后的盘面逐字节一致）。
#
# 假 dumpe2fs 靠工具表变体生效：工具解析在编译期钉死（src/fsops/env.rs 的 TOOL_PATH），
# PATH 影子不参与。BFK 的表把假目录排在系统目录之前，sfdisk 仍落到真身。
source "$(dirname "$0")/lib.sh"
require_fake_bin

img T recheck-mbr.img
HASH=$T.hash-after-tamper

FAKE=$FAKE_TOOL_DIR
cleanup_hook() { rm -rf "$FAKE" 2>/dev/null; rm -rf "$HASH"; }

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

rm_img "$T"
truncate -s 512M "$T"
$B new "$T" --table mbr --yes >/dev/null || { echo "  BAD  fixture: new failed"; rc=1; exit 1; }
$B add "$T" 400M --align mib --yes >/dev/null || { echo "  BAD  fixture: add failed"; rc=1; exit 1; }
$B mkfs "$T":1 ext4 --yes >/dev/null || { echo "  BAD  fixture: mkfs failed"; rc=1; exit 1; }

OUT=$($BFK resize "$T":1 300M --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 5 "首写之前读当前表、不一致即拒绝（记录已落盘）"
if [ ! -s "$HASH" ]; then
    echo "  BAD  注入未触发：fs_min_bytes 探测期间的表改动没有发生"
    rc=1
fi
assert_eq "$(sha256sum "$T" | cut -d' ' -f1)" "$(cut -d' ' -f1 "$HASH")" \
    "拒绝时盘面逐字节不变（整镜像 sha256）"

exit $rc
