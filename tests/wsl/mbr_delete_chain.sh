#!/usr/bin/env bash
# MBR/msdos 全链路 + delete（GPT/MBR）——产品已实现但集成测试零覆盖的路径
# 覆盖：new --table msdos / add --type 0xXX / resize 扩缩 / flag boot|hidden /
#       扩展容器拒绝 / esp flag 拒绝 / delete（--yes 确认、数据区不擦除、GPT 侧）
source "$(dirname "$0")/lib.sh"
require_bin

cleanup_hook() { rm -f /var/lib/diskedit/* 2>/dev/null; }

T=/var/tmp/t29.img
track_file "$T"

rm -f "$T" "$T"$SIDECAR_GLOB 2>/dev/null

echo "===== A: msdos 建表 / add --type / flag ====="
rm -f "$T" "$T"$SIDECAR_GLOB
truncate -s 256M "$T"
$B new "$T" --table msdos --yes; echo "new exit=$?"
$B add "$T" --start 2048 --end 99999 >/dev/null; echo "add p1 exit=$?"
$B add "$T" --start 100000 --end 199999 --type 0x07 >/dev/null; echo "add p2(0x07) exit=$?"
$B info "$T" | grep -o '"label":"[a-z]*"'
$B set "$T":1 flag boot on; echo "flag boot exit=$?"
sfdisk -d "$T" 2>/dev/null | grep -o 'bootable' | head -1
OUT=$($B set "$T":1 flag esp on 2>&1); E=$?
echo "$OUT" | head -1
[ "$E" = "10" ] && echo "flag esp on msdos refused OK (exit=$E)" || { echo "flag esp on msdos wrong exit ($E, want 10)"; rc=1; }
# hidden 仅对 FAT/NTFS 类型有隐藏变体映射
$B set "$T":1 flag hidden on >/dev/null 2>&1; E=$?
[ "$E" = "10" ] && echo "hidden on 0x83 refused OK (exit=$E)" || { echo "hidden on 0x83 wrong exit ($E, want 10)"; rc=1; }
OUT=$($B set "$T":2 flag hidden on 2>&1); echo "$OUT" | head -1; echo "hidden on 0x07 exit=$?"
$B set "$T":2 flag hidden off >/dev/null 2>&1; echo "hidden off exit=$?"

echo
echo "===== B: MBR resize 扩 / 缩 ====="
LD=$(lo_attach "$T")
mkfs.ext4 -q "${LD}p1"
mount_at "${LD}p1" /testmnt && { echo marker-mbr > /testmnt/m.txt; dd if=/dev/urandom of=/testmnt/blob bs=1M count=20 2>/dev/null; umount /testmnt 2>/dev/null; }
lo_detach "$LD"
$B resize "$T":1 48M >/dev/null 2>&1; echo "grow exit=$?"
$B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*'
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt && { cat /testmnt/m.txt; e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "grow e2fsck clean" || { echo "grow e2fsck ISSUES"; rc=1; }; umount /testmnt 2>/dev/null; }
lo_detach "$LD"
$B resize "$T":1 30M >/dev/null 2>&1; echo "shrink exit=$?"
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt && { cat /testmnt/m.txt; umount /testmnt 2>/dev/null; }
e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "shrink e2fsck clean" || { echo "shrink e2fsck ISSUES"; rc=1; }
lo_detach "$LD"

echo
echo "===== C: 扩展容器不可 resize ====="
$B add "$T" --start 200000 --end 393215 --type 0x05 >/dev/null; echo "add extended exit=$?"
$B info "$T" | grep -o '"num":3[^}]*}' | head -1
OUT=$($B resize "$T":3 100M 2>&1); E=$?
echo "$OUT" | head -1
[ "$E" = "10" ] && echo "resize container refused OK (exit=$E)" || { echo "resize container wrong exit ($E, want 10)"; rc=1; }

echo
echo "===== D: delete（数据区不擦除）====="
LD=$(lo_attach "$T")
dd if=/dev/urandom of="${LD}p2" bs=1M count=8 2>/dev/null
MD2=$(head -c 8388608 "${LD}p2" | md5sum | cut -d' ' -f1)
# 分区删除后节点消失，从整盘按 p2 起点 LBA 读同区域比对
# （msdos info 的字段序是 type 在 first_lba 前，需先截取该分区对象再取字段）
P2START=$($B info "$T" | grep -o '"num":2,[^}]*}' | grep -o '"first_lba":[0-9]*' | grep -o '[0-9]*$')
lo_detach "$LD"
OUT=$($B delete "$T":2 2>&1); E=$?
echo "$OUT" | head -1
[ "$E" = "10" ] && echo "del w/o --yes refused OK (exit=$E)" || { echo "del w/o --yes wrong exit ($E, want 10)"; rc=1; }
$B delete "$T":2 --yes >/dev/null 2>&1; echo "del exit=$?"
$B info "$T" | grep -o '"num":2' || echo "p2 GONE OK"
LD=$(lo_attach "$T")
[ "$MD2" = "$(dd if="$LD" bs=512 skip=$P2START count=16384 2>/dev/null | md5sum | cut -d' ' -f1)" ] && echo "p2 DATA-AREA NOT WIPED OK" || { echo "p2 DATA-AREA WIPED (BAD)"; rc=1; }
lo_detach "$LD"

echo
echo "===== E: GPT 侧 delete ====="
rm -f "$T" "$T"$SIDECAR_GLOB
truncate -s 64M "$T"
$B new "$T" --yes >/dev/null
$B add "$T" --start 2048 --end 99999 --name g1 >/dev/null
$B delete "$T":1 --yes >/dev/null 2>&1; echo "gpt del exit=$?"
$B info "$T" | grep -o '"num":1' || echo "g1 GONE OK"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc