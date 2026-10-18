#!/usr/bin/env bash
# 载体守卫与镜像占用闸。两条都只在真块设备/真 loop 上才成立：
# 镜像路径在非 Linux 上恒放行，分区节点的判定也依赖 sysfs。
#
# A. 建表/改表命令的载体必须是整盘 —— 目标是分区节点时一律拒绝，且不得写到那个分区上
# B. 镜像的占用只能按 backing 反查 —— 同一镜像被另一个 loop 挂载时，本工具不得写它
source "$(dirname "$0")/lib.sh"
require_bin

command -v mkfs.ext4 >/dev/null || { echo "SKIP: mkfs.ext4 not installed"; exit 0; }

T=$(mktemp /tmp/carrier-XXXXXX.img)
truncate -s 96M "$T"
track_file "$T"

"$B" new "$T" --yes >/dev/null 2>&1
"$B" create "$T" --size 48M >/dev/null 2>&1

L1=$(lo_attach "$T")
lo_waitpart "${L1}p1" || { echo "SKIP: no partition node on $L1"; exit 0; }
mkfs.ext4 -q "${L1}p1" >/dev/null 2>&1
e2fsck -fn "${L1}p1" >/dev/null 2>&1
exp "$?" 0 "基线：文件系统干净"

echo "=== A: 分区节点不是建表/改表命令的载体"
"$B" del "${L1}p1:1" --yes >/dev/null 2>&1
exp "$?" 10 "del 打分区节点被拒"
# 拒绝必须是"什么都没写"：判据只认 0x55AA 时这里会清零分区 LBA0 的 [446,462)
e2fsck -fn "${L1}p1" >/dev/null 2>&1
exp "$?" 0 "分区内容未被清零"
"$B" resize-part "${L1}p1:1" --start 2048 --end 4095 >/dev/null 2>&1
exp "$?" 10 "resize-part 打分区节点被拒"

echo "=== B: 同一镜像的另一个 loop 已挂载"
L2=$(lo_attach "$T")
lo_waitpart "${L2}p1"
mount_at "${L1}p1" /testmnt || { echo "SKIP: mount failed"; exit 0; }
"$B" resize-part "$T:1" --start 2048 --grow-to-end >/dev/null 2>&1
exp "$?" 10 "挂载中拒绝写同一镜像"
unmount_now /testmnt
"$B" resize-part "$T:1" --start 2048 --grow-to-end >/dev/null 2>&1
exp "$?" 0 "卸载后放行（阳性对照）"
e2fsck -fn "${L1}p1" >/dev/null 2>&1
exp "$?" 0 "文件系统无损伤"

echo "=== C: 挂载镜像上的直接改表命令"
# 直接改表的每条命令都要在首个字节落盘之前拒绝：分区表与内核仍在使用的
# 分区视图分叉后，后续写入会落到"表说不存在、内核仍映射"的区间上。
# B 段把分区扩到盘尾，内核里 L1 的分区尺寸仍是扩前值（改表不触发 BLKRRPART），
# 直接拿 L1p1 挂载会与超块尺寸不符；换一个新 loop 重读盘面再挂
losetup -d "$L1" "$L2" 2>/dev/null
L3=$(lo_attach "$T")
lo_waitpart "${L3}p1" || { echo "BAD: $T 上无分区节点 $L3p1"; exit 1; }
mount_at "${L3}p1" /testmnt || { echo "BAD: 挂载 $L3p1 失败"; exit 1; }
before=$(sha256sum "$T" | awk '{print $1}')
check_refused() {
    local label=$1 out got
    shift
    out=$("$@" 2>&1)
    got=$?
    exp "$got" 10 "$label"
    case "$out" in
        *"is in use"*) echo "  OK   $label：拒绝原因落在 loop 在用分支" ;;
        *) echo "  BAD  $label：拒绝原因不符: $out"; rc=1 ;;
    esac
}
check_refused "new"       "$B" new "$T" --yes
check_refused "add"       "$B" add "$T" --start 102400 --end 103423
check_refused "del"       "$B" del "$T:1" --yes
check_refused "create"    "$B" create "$T" --size 8M
check_refused "set name"  "$B" set "$T:1" name foo
check_refused "set type"  "$B" set "$T:1" type C12A7328-F81F-11D2-BA4B-00A0C93EC93B
check_refused "set flag"  "$B" set "$T:1" flag hidden on
check_refused "undo"      "$B" undo "$T" --yes
after=$(sha256sum "$T" | awk '{print $1}')
assert_eq "$after" "$before" "拒绝期间镜像字节未改变"
unmount_now /testmnt
"$B" new "$T" --yes >/dev/null 2>&1
exp "$?" 0 "卸载后 new 放行（阳性对照）"

exit $rc