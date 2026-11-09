#!/usr/bin/env bash
# last_usable 边界：1GiB GPT 的可用区为 34..2097118
# 判定：逐条断言退出码；越界请求必须是 3（拒绝），且拒绝不得改动表
source "$(dirname "$0")/lib.sh"
require_bin

img T t28.img
LU=2097118   # 1GiB/512 = 2097152 扇区；去掉备份数组(32)与备份头(1)后的末个可用 LBA

p1_last() { last_lba1 "$T"; } # p1 的 last_lba
last_lba1() { # $1=镜像：分区 1 的 last_lba
  $B info "$1" | grep -o '"num":1,[^}]*}' | grep -o '"last_lba":[0-9]*' | cut -d: -f2
}
fs_bytes() { # $1=设备：Block count × Block size
  dumpe2fs -h "$1" 2>/dev/null | awk '/^Block count:/ {c=$3} /^Block size:/ {s=$3} END {print c*s}'
}

rm -rf "$T" "$T"$SIDECAR_GLOB 2>/dev/null
truncate -s 1G "$T"

echo "== new（1GiB）=="
$B new "$T" --yes >/dev/null; exp $? 0 "new"

echo "== add p1（2048..4095）=="
$B add --yes "$T" 1048576 --start 2048 --name p1 >/dev/null; exp $? 0 "add p1（2048..4095）"
# resize-part 的扩容自带 FS 步：未识别内容的 grow 在写盘前被拒（6）。本段验的是
# 边界几何，给 p1 建一个 ext4 让 FS 步成立，几何断言不受影响
$B mkfs --yes "$T":1 ext4 >/dev/null 2>&1 || { echo "  BAD  mkfs p1 失败"; rc=1; }

echo "== 扩到 last_usable =="
$B resize-part --yes "$T":1 --grow-to-end >/dev/null; exp $? 0 "at last_usable"
[ "$(p1_last)" = "$LU" ] && echo "  OK   p1 已到 2048..$LU" \
  || { echo "  BAD  p1 未到 last_usable（last_lba=$(p1_last)）"; rc=1; }

echo "== 越过 last_usable 必须拒绝 =="
# 三条都必须显式 --align none：--align mib 会把起点向上对齐（src/support.rs 的
# align_start），越界判断拿到的就不是原始 LBA，断言可能"因错因通过"。
# 加 --align none 后原始起点与长度直达校验：resize-part 命中 src/movepart/resize.rs:404 的
# `new_end > last_usable_lba`，add 命中 src/cmd/layout.rs 的 fixed_start 可用区判定，
# 拒绝才是因越界。
# 反例佐证：uncovered_branches.sh 的紧贴边界用例同样使用 --align none。
$B resize-part --yes "$T":1 --align none --end $((LU + 1)) >/dev/null 2>&1; exp $? 3 "last_usable+1 拒绝"
# 2048..2097150 共 2095103 扇区 × 512 = 1072692736 字节；终点越过 last_usable
$B add --yes "$T" 1072692736 --start 2048 --align none >/dev/null 2>&1; exp $? 3 "beyond last_usable 拒绝"
# 起点取 LU+1，终点仍按 2097150 收：长度 = (2097150 - (LU+1) + 1) 扇区
$B add --yes "$T" $(( (2097150 - (LU + 1) + 1) * 512 )) --start $((LU + 1)) --align none >/dev/null 2>&1; exp $? 3 "start 越界拒绝"

echo "== 拒绝不得改动表 =="
[ "$(p1_last)" = "$LU" ] && echo "  OK   拒绝未改动表" \
  || { echo "  BAD  拒绝路径改动了表"; rc=1; }

# ---- 在线路径（块设备 + 挂载中分区）----
# 上面几段验的是几何入口（resize-part / add）的越界；`resize <disk>:N` 走的是**另一条**
# 入口：在线分支在锁下按盘上几何重算目标，边界必须由本工具拒绝，不能交给 sfdisk --force
# 兜底（它的一致性检查被本工具主动关掉了）
img T2 t28online.img
MP=$TEST_MNT
rm -rf "$T2" "$T2"$SIDECAR_GLOB 2>/dev/null

echo "== 造 1GiB GPT + ext4 p1（2048..$LU，吃满可用区）=="
truncate -s 1G "$T2"
$B new "$T2" --yes >/dev/null; exp $? 0 "new（在线段）"
$B add --yes "$T2" $(( (LU - 2048 + 1) * 512 )) --start 2048 --align none --name p1 >/dev/null; exp $? 0 "add p1（2048..$LU）"
$B mkfs "$T2":1 ext4 --yes >/dev/null 2>&1; exp $? 0 "mkfs p1 ext4"

LD2=$(lo_attach "$T2")
mount_at "${LD2}p1" "$MP" || { echo "  BAD  mount 失败"; rc=1; exit 1; }
LAST0=$(last_lba1 "$T2")
FS0=$(fs_bytes "${LD2}p1")

echo "== 在线 grow：可用区已到顶，拒绝 =="
# 理由必须落在"分区层没有可扩空间"，不得断言"文件系统到顶"——FS 仍可能小于分区
$B resize --yes "$LD2":1 grow >/dev/null 2>&1; exp $? 3 "resize grow（已顶到 last_usable）拒绝"
$B resize --yes "$LD2":1 grow 2>&1 >/dev/null | grep -q "would not extend the partition" \
  && echo "  OK   拒绝理由落在分区层" || { echo "  BAD  拒绝理由不在分区层"; rc=1; }
exp "$(last_lba1 "$T2")" "$LAST0" "  拒绝后分区表未变"

echo "== 在线绝对尺寸：终点越过 last_usable 一个扇区，拒绝 =="
# 终点取 LU+1：仍在盘内（盘尾 LBA = 2097151），故 online 层 check_new_range 的容量查不会
# 拦下它——拦它的必须是本工具自己的可用区上界
OVERSIZE=$(( (LU + 1 - 2048 + 1) * 512 ))
$B resize --yes "$LD2":1 "$OVERSIZE" >/dev/null 2>&1; exp $? 3 "resize（终点 LU+1）拒绝"
$B resize --yes "$LD2":1 "$OVERSIZE" 2>&1 >/dev/null | grep -q "exceeds the usable range" \
  && echo "  OK   拒绝理由指向可用区" || { echo "  BAD  拒绝理由不是可用区"; rc=1; }
exp "$(last_lba1 "$T2")" "$LAST0" "  拒绝后分区表未变"
exp "$(fs_bytes "${LD2}p1")" "$FS0" "  拒绝后 FS 未变"

umount "$MP"

echo
echo "==== last_usable 边界结束（rc=$rc）===="
exit $rc