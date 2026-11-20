#!/usr/bin/env bash
# LVM PV 缩容（镜像目标走离线路径）：末端的已分配 extent 都在新末端之内时，
# 先 pvresize --setphysicalvolumesize 缩 PV 元数据，再提交分区表。
# 依赖 lvm2；WSL 未装则跳过，不安装以免弄脏环境
source "$(dirname "$0")/lib.sh"

if ! command -v pvcreate >/dev/null 2>&1; then
  echo "SKIP: lvm2 not installed (pvcreate missing) — 按约定不在 WSL 安装依赖"
  exit 0
fi

require_bin

cleanup_hook() {
  vgchange -an vgshrink 2>/dev/null
  vgremove -f vgshrink 2>/dev/null
  pvremove -f "${LD}p1" 2>/dev/null
  lo_detach "$LD" 2>/dev/null
}

img T t34.img
rm_img "$T" 2>/dev/null
truncate -s 1G "$T"
$B new "$T" --yes >/dev/null
$B add --yes "$T" 700M --start 2048 --name pv0 --type-guid E6D6D379-F507-44C2-A23C-238F2A3DF928 >/dev/null
exp $? 0 "add pv0"

LD=$(lo_attach "$T")
lo_waitpart "${LD}p1" || { echo "loop partition node missing"; exit 1; }
pvcreate "${LD}p1" >/dev/null && echo "pvcreate OK"
vgcreate vgshrink "${LD}p1" >/dev/null && echo "vgcreate OK"
lvcreate -L 200M -n lv1 vgshrink >/dev/null && echo "lvcreate OK"
mkfs.ext4 -q /dev/vgshrink/lv1
mount_at /dev/vgshrink/lv1 "$TEST_MNT" && { echo pv-marker > "$TEST_MNT"/m.txt; unmount_now "$TEST_MNT"; }
# pvresize 缩容要求 VG 不活跃：活动 LV 经 dm 持有分区，占用闸会拒绝。
# udev 的 LVM 规则会在 PV 设备出现时自动激活 VG（69-lvm.rules 的 vgchange -aay），
# 故另把 LV 标成 activation skip，否则 vgchange -an 之后仍会被自动拉起
lvchange -ky vgshrink/lv1 >/dev/null && echo "lvchange -ky OK"
vgchange -an vgshrink >/dev/null && echo "vgchange -an OK"
PV_BEFORE=$(pvs --noheadings --nosuffix --units b -o pv_size "${LD}p1" | tr -d ' ')
P1_BEFORE=$($B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*')
# 产品会为镜像自建 loop：先释放本脚本的映射，否则同一 PV 出现在两个设备上，
# pvs 只认其中一个，回读会拿到重复映射的占位值
lo_detach "$LD"

echo "-- PV 缩到 500M：先 pvresize、后提交表项 --"
$B resize --yes "$T":1 500M >/dev/null 2>&1; exp $? 0 "resize 500M（PV 缩容）"
LD=$(lo_attach "$T")
lo_waitpart "${LD}p1" || { echo "loop partition node missing"; exit 1; }
PV_AFTER=$(pvs --noheadings --nosuffix --units b -o pv_size "${LD}p1" | tr -d ' ')
[ "$PV_AFTER" -le 524288000 ] && echo "  OK   pv_size=$PV_AFTER <= 524288000" \
  || { echo "  BAD  pv_size=$PV_AFTER > 524288000"; rc=1; }
[ "$PV_AFTER" -le "$PV_BEFORE" ] && echo "  OK   pv_size 未增大（$PV_BEFORE → $PV_AFTER）" \
  || { echo "  BAD  pv_size 增大（$PV_BEFORE → $PV_AFTER）"; rc=1; }
assert_eq "$($B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*')" \
  '"num":1,"first_lba":2048,"last_lba":1026047' "  分区末端落在 500M"

echo "-- LV 数据仍在，重新激活后可挂载 --"
lvchange -kn vgshrink/lv1 >/dev/null 2>&1
lvchange -ay vgshrink/lv1 >/dev/null && echo "lvchange -ay OK"
for _ in {1..50}; do [ -b /dev/vgshrink/lv1 ] && break; sleep 0.1; done
if mount_at /dev/vgshrink/lv1 "$TEST_MNT"; then
  assert_eq "$(cat "$TEST_MNT"/m.txt 2>/dev/null)" "pv-marker" "  LV 数据完整"
  unmount_now "$TEST_MNT"
else
  echo "  BAD  LV 挂载失败"; rc=1
fi
e2fsck -fn /dev/vgshrink/lv1 >/dev/null 2>&1 && echo "LV e2fsck clean" || { echo "LV e2fsck ISSUES"; rc=1; }
vgchange -an vgshrink >/dev/null 2>&1
sgdisk_check "$T"
exit $rc
