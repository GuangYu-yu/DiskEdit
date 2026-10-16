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

exit $rc