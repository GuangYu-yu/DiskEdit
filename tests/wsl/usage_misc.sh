#!/usr/bin/env bash
# 剩余用法：--grow-to-end / copy --start end / 百分比 / set 组合
source "$(dirname "$0")/lib.sh"
require_bin

T=/var/tmp/t21.img
track_file "$T"

mk() {
  rm -f "$T" "$T".diskedit.*; truncate -s 1G "$T"
  $B new "$T" --yes >/dev/null
  $B create "$T" --size 100M --name p1 --fs ext4 >/dev/null
  $B create "$T" --size 100M --name p2 --fs ext4 >/dev/null
  $B create "$T" --size 200M --name p3 --fs ext4 >/dev/null
}
fill() {
  LD=$(lo_attach "$T"); mount_at "${LD}p$1" /testmnt
  dd if=/dev/urandom of=/testmnt/blob bs=1M count=$2 2>/dev/null
  umount /testmnt; md5sum "${LD}p$1" | cut -d' ' -f1; losetup -d "$LD"
}
chk() {
  LD=$(lo_attach "$T") || return
  if [ "$2" != "-" ]; then
    [ "$2" = "$(md5sum "${LD}p$1" | cut -d' ' -f1)" ] && echo "p$1 DEV OK" || { echo "p$1 DEV CORRUPT"; rc=1; }
  fi
  e2fsck -fn "${LD}p$1" >/dev/null 2>&1 && echo "p$1 e2fsck clean" || { echo "p$1 e2fsck ISSUES"; rc=1; }
  losetup -d "$LD"
}

echo "=== 1: resize-part --grow-to-end（末分区扩到尾） ==="
mk; M3=$(fill 3 190)
# resize-part 的 --start 是必填：缺它直接走 usage() 出口退 10（src/cmd/layout.rs:164），
# 与"扩容被拒"是两回事。p3 的起点从盘上读，保持原测试意图（末分区扩到可用区尾）
P3S=$($B info "$T" | grep -o '"num":3,[^}]*}' | grep -o '"first_lba":[0-9]*' | cut -d: -f2)
$B resize-part "$T":3 --start "$P3S" --grow-to-end >/dev/null 2>&1; exp $? 0 "resize-part --grow-to-end"
$B info "$T" | grep -o '"num":3,"first_lba":[0-9]*,"last_lba":[0-9]*'
chk 3 -

echo
echo "=== 2: copy --start end（复制到尾部） ==="
mk; M1=$(fill 1 90)
$B copy "$T":1 --start end >/dev/null 2>&1; exp $? 0 "copy --start end"
$B info "$T" | grep -o '"num":4,"first_lba":[0-9]*,"last_lba":[0-9]*'
chk 1 "$M1"
LD=$(lo_attach "$T"); [ "$M1" = "$(md5sum "${LD}p4" | cut -d' ' -f1)" ] && echo "p4 COPY OK (device-identical)" || { echo "p4 COPY MISMATCH"; rc=1; }; losetup -d "$LD"

echo
echo "=== 3: 百分比 +10% ==="
mk; M3=$(fill 3 190)
$B resize "$T":3 +10% >/dev/null 2>&1; exp $? 0 "resize +10%"
$B info "$T" | grep -o '"num":3,"first_lba":[0-9]*,"last_lba":[0-9]*'
echo "  (200M × 110% = 220M = 450560 扇区)"
chk 3 -

echo
echo "=== 4: set name/label/flag 组合 ==="
mk
$B set "$T":1 name renameme 2>&1 | head -1
$B set "$T":1 label ROOTFS 2>&1 | head -1
$B set "$T":1 flag esp on 2>&1 | head -1
$B info "$T" | grep -o '"num":1[^}]*}' | head -1
$B set "$T":1 flag esp off 2>&1 | head -1
$B set "$T":1 uuid 12345678-1234-1234-1234-123456789abc 2>&1 | head -1
blkid -s LABEL -o value /dev/null 2>/dev/null
LD=$(lo_attach "$T"); blkid -s LABEL -o value "${LD}p1"; losetup -d "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc