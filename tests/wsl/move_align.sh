#!/usr/bin/env bash
# move --start align：从分区现有起点上取整到下一个 1 MiB 边界（GPT 独有）
# 覆盖：未对齐首分区搬移后内容完好 / 已对齐时 no-op / 与 --align 互斥 /
#       add、copy 没有可对齐的起点 / 落点撞上邻居时按范围重叠拒绝
source "$(dirname "$0")/lib.sh"
require_bin

img T align.img

echo "===== A: 起点 63 移到 1 MiB 边界 ====="
rm_img "$T"
truncate -s 256M "$T"
$B new "$T" --yes >/dev/null; exp "$?" 0 "new"
$B add --yes "$T" 40M --start 63 >/dev/null; exp "$?" 0 "add p1 @63"
$B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*'
LD=$(lo_attach "$T")
mkfs.ext4 -q "${LD}p1"
mount_at "${LD}p1" "$TEST_MNT" && { echo marker-align > "$TEST_MNT"/m.txt; dd if=/dev/urandom of="$TEST_MNT"/blob bs=1M count=8 2>/dev/null; umount "$TEST_MNT" 2>/dev/null; }
lo_detach "$LD"

OUT=$($B move "$T":1 --start align --yes 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 0 "move --start align"
$B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*'
START=$($B info "$T" | grep -o '"first_lba":[0-9]*' | head -1 | grep -o '[0-9]*$')
[ "$START" = "2048" ] && echo "aligned start OK ($START)" || { echo "aligned start WRONG ($START, want 2048)"; rc=1; }
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$TEST_MNT" && { cat "$TEST_MNT"/m.txt; e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "align e2fsck clean" || { echo "align e2fsck ISSUES"; rc=1; }; umount "$TEST_MNT" 2>/dev/null; }
lo_detach "$LD"

echo
echo "===== B: 已对齐时是 no-op ====="
OUT=$($B move "$T":1 --start align --yes 2>&1); E=$?
echo "$OUT" | tail -1
exp "$E" 0 "already aligned"
$B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*'

echo
echo "===== C: --align 与 align 选择器互斥 ====="
OUT=$($B move "$T":1 --start align --align mib --yes 2>&1); E=$?
echo "$OUT" | head -1
[ "$E" = "3" ] && echo "align + --align refused OK (exit=$E)" || { echo "align + --align wrong exit ($E, want 3)"; rc=1; }

echo
echo "===== D: add / copy 没有可对齐的起点 ====="
OUT=$($B add --yes "$T" 8M --start align 2>&1); E=$?
echo "$OUT" | head -1
[ "$E" = "3" ] && echo "add --start align refused OK (exit=$E)" || { echo "add --start align wrong exit ($E, want 3)"; rc=1; }
OUT=$($B copy "$T":1 --start align --yes 2>&1); E=$?
echo "$OUT" | head -1
[ "$E" = "3" ] && echo "copy --start align refused OK (exit=$E)" || { echo "copy --start align wrong exit ($E, want 3)"; rc=1; }

echo
echo "===== E: 右侧紧邻分区挡住对齐落点 ====="
rm_img "$T"
truncate -s 128M "$T"
$B new "$T" --yes >/dev/null; exp "$?" 0 "new"
$B add --yes "$T" 40M --start 63 >/dev/null; exp "$?" 0 "add p1 @63"
$B add --yes "$T" 8M --start 81983 >/dev/null; exp "$?" 0 "add p2 adjacent"
OUT=$($B move "$T":1 --start align --yes 2>&1); E=$?
echo "$OUT" | head -1
[ "$E" = "3" ] && echo "$OUT" | grep -q "would overlap" && echo "neighbour overlap refused OK" \
  || { echo "neighbour overlap wrong (exit=$E)"; rc=1; }

exit $rc
