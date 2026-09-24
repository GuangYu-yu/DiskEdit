#!/usr/bin/env bash
# 4Kn（4096 逻辑扇区）—— 经**块设备**路径（工具从设备探测逻辑扇区大小）
# 残留控制：t24 前缀 + trap 自清理
source "$(dirname "$0")/lib.sh"
require_fault_bin

T=/var/tmp/t24.img
LD=""

cleanup_hook() {
  [ -n "$LD" ] && lo_detach "$LD" 2>/dev/null
  rm -f /var/tmp/t24*.img* 2>/dev/null
  rm -f /var/lib/diskedit/* 2>/dev/null
}

rm -f /var/tmp/t24*.img* 2>/dev/null
rm -f /var/lib/diskedit/* 2>/dev/null
track_file "$T"

refresh() { sync; blockdev --rereadpt "$LD" 2>/dev/null; sleep 0.3; }

setup4k() {
  rm -f "$T" "$T".diskedit.*; rm -f /var/lib/diskedit/* 2>/dev/null
  [ -n "$LD" ] && lo_detach "$LD" 2>/dev/null
  truncate -s 2G "$T"
  LD=$(lo_attach "$T" --sector-size 4096)
  $B new "$LD" --yes >/dev/null
  # 4Kn 下 1MiB = 256 扇区
  $B add "$LD" --start 256 --end 65791 --name p1 >/dev/null; refresh; mkfs.ext4 -q "${LD}p1"
  $B add "$LD" --start 131072 --end 196607 --name p2 >/dev/null; refresh; mkfs.ext4 -q "${LD}p2"
  $B add "$LD" --start 262144 --end 327679 --name p3 >/dev/null; refresh; mkfs.ext4 -q "${LD}p3"
}

echo "########## A: 块设备 4Kn 识别 ##########"
setup4k
echo "loop: $LD  logical_ss=$(blockdev --getss "$LD")"
$B info "$LD" | head -c 130; echo
echo "-- 期望 sector_size=4096 且 LBA 与 256 对齐 --"
$B info "$LD" | grep -o '"num":[0-9]*,"first_lba":[0-9]*,"last_lba":[0-9]*'
mount_at "${LD}p2" /testmnt && { dd if=/dev/urandom of=/testmnt/blob bs=1M count=200 2>/dev/null; echo "p2 200M written"; umount /testmnt; M2=$(md5sum "${LD}p2" | cut -d' ' -f1); }

echo
echo "########## B: 4Kn 自重叠左移（delta < 分区长度）##########"
$B move "$LD":2 --start 66048 --chunk-size 4 >/dev/null 2>&1; echo "move exit=$?"
$B info "$LD" | grep -o '"num":2,[^}]*}'
refresh
e2fsck -fn "${LD}p2" >/dev/null 2>&1; echo "e2fsck -fn exit=$? (0=clean)"
[ "$M2" = "$(md5sum "${LD}p2" | cut -d' ' -f1)" ] && echo "4KN DEV OK" || { echo "4KN DEV CORRUPT"; rc=1; }
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; sgdisk -v "$T" 2>&1 | tail -2; }

echo
echo "########## C: 4Kn 中断恢复（fault 注入）##########"
setup4k
mount_at "${LD}p2" /testmnt && { dd if=/dev/urandom of=/testmnt/blob bs=1M count=200 2>/dev/null; umount /testmnt; M2=$(md5sum "${LD}p2" | cut -d' ' -f1); }
DISKEDIT_FAULT=rs-chunk:20 $BF move "$LD":2 --start 66048 --chunk-size 4 >/dev/null 2>&1
OUT=$($B move "$LD":2 --start 66048 --chunk-size 4 2>&1)
echo "resume: $(echo "$OUT" | grep -o 'resuming at chunk [0-9]*')"
refresh
[ "$M2" = "$(md5sum "${LD}p2" | cut -d' ' -f1)" ] && echo "4KN RESUME DEV OK" || { echo "4KN RESUME DEV CORRUPT"; rc=1; }
e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "4KN RESUME e2fsck clean" || { echo "4KN RESUME e2fsck ISSUES"; rc=1; }

echo
echo "########## D: 分区身份/属性搬移后保持（4Kn 块设备）##########"
setup4k
$B set "$LD":2 name keepname >/dev/null
$B set "$LD":2 flag esp on >/dev/null
PU_BEFORE=$(sgdisk -i 2 "$LD" 2>/dev/null | grep -i "unique GUID" | awk '{print $NF}')
TY_BEFORE=$($B info "$LD" | grep -o '"num":2,[^}]*}' | grep -o '"type":"[^"]*"')
$B move "$LD":2 --start 66048 --chunk-size 4 >/dev/null 2>&1; echo "move exit=$?"
PU_AFTER=$(sgdisk -i 2 "$LD" 2>/dev/null | grep -i "unique GUID" | awk '{print $NF}')
TY_AFTER=$($B info "$LD" | grep -o '"num":2,[^}]*}' | grep -o '"type":"[^"]*"')
NAME_AFTER=$($B info "$LD" | grep -o '"num":2,[^}]*}' | grep -o '"name":"[^"]*"')
[ "$PU_BEFORE" = "$PU_AFTER" ] && [ -n "$PU_BEFORE" ] && echo "D PARTUUID PRESERVED OK" || { echo "D PARTUUID CHANGED ($PU_BEFORE -> $PU_AFTER)"; rc=1; }
[ "$TY_BEFORE" = "$TY_AFTER" ] && echo "D TYPE-GUID(esp) PRESERVED OK" || { echo "D TYPE-GUID CHANGED"; rc=1; }
echo "$NAME_AFTER" | grep -q keepname && echo "D NAME PRESERVED OK" || { echo "D NAME LOST"; rc=1; }

echo
echo "########## 残留核对（cleanup 前）##########"
echo "t24 files: $(ls /var/tmp/t24* 2>/dev/null | wc -l)  loops: $(losetup -a | wc -l)"
exit $rc