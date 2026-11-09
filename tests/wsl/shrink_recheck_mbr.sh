#!/usr/bin/env bash
# 回归：MBR 缩容在命令层即被拒（resize 只扩），FS 探测与任何写盘都不会发生。
#
# 假 dumpe2fs 与 GPT 版同一注入手法：被调到才改表并记账。命令层拒绝发生在
# 任何探测之前——记账文件不存在即是"探测未触达"的物证，镜像保持原字节。
# GPT 路径的探测窗口契约（首写之前复核当前表）由 shrink_recheck_gpt.sh 承担。
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

rm -rf "$T" "$T"$SIDECAR_GLOB
truncate -s 512M "$T"
$B new "$T" --table mbr --yes >/dev/null || { echo "  BAD  fixture: new failed"; rc=1; exit 1; }
$B add "$T" 400M --align mib --yes >/dev/null || { echo "  BAD  fixture: add failed"; rc=1; exit 1; }
$B mkfs "$T":1 ext4 --yes >/dev/null || { echo "  BAD  fixture: mkfs failed"; rc=1; exit 1; }

OUT=$($BFK resize "$T":1 300M --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | head -2 | sed 's/^/    /'
exp "$E" 3 "MBR 缩容在命令层被拒（resize 只扩），未触达任何探测"
if [ -s "$HASH" ]; then
    echo "  BAD  被拒的命令不应触达 FS 探测（表改动发生了）"
    rc=1
fi
exp "$(record_absent "$T" && echo yes || echo no)" yes "拒绝不留记录"

exit $rc
