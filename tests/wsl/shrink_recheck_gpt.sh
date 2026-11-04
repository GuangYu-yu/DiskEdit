#!/usr/bin/env bash
# 回归：GPT 离线缩容的判据必须在首写之前成立。
#
# 窗口取在 fs_min_bytes 探测期间（prepare 已读出决策几何、shrink_fs 尚未启动）：
# 假 dumpe2fs 先把盘上目标条目改成请求尺寸，再把改动后的整镜像摘要记下。
# 旧实现会先缩 FS、再在阶段 1 复核时发现表已变（30，镜像已被改写）；
# 新实现必须在任何写入之前拒绝（10，镜像与改动后的盘面逐字节一致）。
#
# 假 dumpe2fs 靠工具表变体生效：工具解析在编译期钉死（src/fsops/env.rs 的 TOOL_PATH），
# PATH 影子不参与。BFK 的表把假目录排在系统目录之前，sfdisk 仍落到真身。
source "$(dirname "$0")/lib.sh"
require_fake_bin

T=/var/tmp/recheck-gpt.img
HASH=$T.hash-after-tamper
track_file "$T"

FAKE=$FAKE_TOOL_DIR
cleanup_hook() { rm -rf "$FAKE" 2>/dev/null; rm -f "$HASH"; }

rm -rf "$FAKE"; mkdir -p "$FAKE"
# 第一次调用是 fs_min_bytes 的 Block size 探测：先改表再记账。之后的调用只输出回读字段
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
$B new "$T" --yes >/dev/null
$B create "$T" --size 400M --name p1 --fs ext4 --yes >/dev/null 2>&1

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
