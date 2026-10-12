#!/usr/bin/env bash
# PV 在线扩容（分区层）：活跃 LV 经 device-mapper 持有分区使 BLKRRPART EBUSY，
# 必须走 sfdisk+partx 路径（src/main.rs resize_pv_online）。
# 依赖 lvm2（pvcreate/vgcreate/lvcreate）——WSL 未装则跳过，不安装以免弄脏环境
source "$(dirname "$0")/lib.sh"

if ! command -v pvcreate >/dev/null 2>&1; then
  echo "SKIP: lvm2 not installed (pvcreate missing) — 按约定不在 WSL 安装依赖"
  exit 0
fi

require_bin

# LVM 栈须先拆（LV→VG→PV），再释放本脚本的 loop
cleanup_hook() {
  vgchange -an vgtest 2>/dev/null
  vgremove -f vgtest 2>/dev/null
  pvremove -f "$T" 2>/dev/null
  lo_detach "$LD" 2>/dev/null
}

T=/var/tmp/t32.img
track_file "$T"

rm -f "$T" "$T"$SIDECAR_GLOB 2>/dev/null
truncate -s 1G "$T"
$B new "$T" --yes >/dev/null
# LVM 分区类型 GUID
$B add "$T" --start 2048 --end 999999 --name pv0 --type E6D6D379-F507-44C2-A23C-238F2A3DF928 >/dev/null; exp $? 0 "add pv0"

LD=$(lo_attach "$T")
pvcreate "${LD}p1" >/dev/null && echo "pvcreate OK"
vgcreate vgtest "${LD}p1" >/dev/null && echo "vgcreate OK"
lvcreate -L 200M -n lv1 vgtest >/dev/null && echo "lvcreate OK"
mkfs.ext4 -q /dev/vgtest/lv1
mount_at /dev/vgtest/lv1 /testmnt && { echo lv-marker > /testmnt/m.txt; umount /testmnt 2>/dev/null; }
lvchange -ay /dev/vgtest/lv1 2>/dev/null
echo "PV before: $(pvs --noheadings -o pv_size "${LD}p1" 2>/dev/null | tr -d ' ')"

echo "-- 活跃 LV 持有分区时 resize 分区层（应走 sfdisk+partx，不因 EBUSY 失败）--"
$B resize "$LD":1 800M >/dev/null 2>&1; exp $? 0 "分区层扩容 800M（走 sfdisk+partx）"
$B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*'
echo "kernel 视图: $(blockdev --getsize64 "${LD}p1") bytes (期望 838860800)"

echo "-- 越界目标（终点 = 容器末个 LBA，越过 last_usable_lba）：应用自身拒绝，不改表 --"
# PV 与非 PV 共用同一个在线目标解析：目标终点越过可用区必须由本工具拒绝（10，与离线同
# 口径），不能把边界交给 sfdisk --force 兜底。终点取容器末个 LBA——仍在盘内，故
# online 层 check_new_range 的容量查不会拦下它，拦它的只能是可用区上界
LAST_LBA=$(( $(stat -c %s "$T") / 512 - 1 ))
OVERSIZE=$(( (LAST_LBA - 2048 + 1) * 512 ))
P1_BEFORE=$($B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*')
$B resize "$LD":1 "$OVERSIZE" >/dev/null 2>&1; exp $? 10 "PV 越界目标拒绝"
$B resize "$LD":1 "$OVERSIZE" 2>&1 >/dev/null | grep -q "exceeds the usable range" \
  && echo "  OK   拒绝理由指向可用区" || { echo "  BAD  拒绝理由不是可用区"; rc=1; }
assert_eq "$($B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*')" "$P1_BEFORE" "  拒绝后分区表未变"

echo "-- LV 数据不受影响，分区层已扩、pvresize 留给调用方 --"
mount_at /dev/vgtest/lv1 /testmnt && { cat /testmnt/m.txt; umount /testmnt 2>/dev/null; }
e2fsck -fn /dev/vgtest/lv1 >/dev/null 2>&1 && echo "LV e2fsck clean" || { echo "LV e2fsck ISSUES"; rc=1; }
echo "PV after: $(pvs --noheadings -o pv_size "${LD}p1" 2>/dev/null | tr -d ' ')（应仍为旧值，pvresize 归调用方）"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc