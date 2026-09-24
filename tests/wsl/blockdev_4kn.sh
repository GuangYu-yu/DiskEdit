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

refresh() { sync; blockdev --rereadpt "$LD" 2>/dev/null; partx -u "$LD" 2>/dev/null; sleep 1; }

# 表写命令的稳定执行：DiskEdit 在表提交后校验内核分区视图，偶发 reread 竞态会以
# "kernel partition view is stale" 失败并保留 journal（表已写、可 undo）——错误消息
# 本身指引 partprobe/刷新内核视图，这里照做后重试一次，消除测试自身的时序赌运气
set4kn() { # $1=label  其余=传给 set 的参数
  local label=$1 out e; shift
  out=$($B set "$LD":2 "$@" 2>&1); e=$?
  if [ "$e" -ne 0 ] && echo "$out" | grep -q 'stale'; then
    partx -u "$LD" 2>/dev/null; sleep 1
    out=$($B set "$LD":2 "$@" 2>&1); e=$?
  fi
  [ "$e" -eq 0 ] && echo "  OK   set $label" || { echo "  BAD  set $label FAILED: $(echo "$out" | tail -1)"; rc=1; }
}

setup4k() {
  rm -f "$T" "$T".diskedit.*; rm -f /var/lib/diskedit/* 2>/dev/null
  [ -n "$LD" ] && lo_detach "$LD" 2>/dev/null
  truncate -s 2G "$T"
  LD=$(lo_attach "$T" --sector-size 4096)
  $B new "$LD" --yes >/dev/null || { echo "setup: new FAILED"; rc=1; return 1; }
  # 4Kn 下 1MiB = 256 扇区
  $B add "$LD" --start 256 --end 65791 --name p1 >/dev/null || { echo "setup: add p1 FAILED"; rc=1; return 1; }
  refresh; mkfs.ext4 -q "${LD}p1" || { echo "setup: mkfs p1 FAILED"; rc=1; return 1; }
  $B add "$LD" --start 131072 --end 196607 --name p2 >/dev/null || { echo "setup: add p2 FAILED"; rc=1; return 1; }
  refresh; mkfs.ext4 -q "${LD}p2" || { echo "setup: mkfs p2 FAILED"; rc=1; return 1; }
  $B add "$LD" --start 262144 --end 327679 --name p3 >/dev/null || { echo "setup: add p3 FAILED"; rc=1; return 1; }
  refresh; mkfs.ext4 -q "${LD}p3" || { echo "setup: mkfs p3 FAILED"; rc=1; return 1; }
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
echo "现场：$(ls /var/lib/diskedit/ 2>/dev/null | wc -l) 项"
OUT=$($B move "$LD":2 --start 66048 --chunk-size 4 2>&1)
R=$(echo "$OUT" | grep -o 'resuming at chunk [0-9]*')
[ "$R" = "resuming at chunk 20" ] && echo "resume: $R" || { echo "resume: WRONG ($R，输出尾: $(echo "$OUT" | tail -1))"; rc=1; }
refresh
[ "$M2" = "$(md5sum "${LD}p2" | cut -d' ' -f1)" ] && echo "4KN RESUME DEV OK" || { echo "4KN RESUME DEV CORRUPT"; rc=1; }
e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "4KN RESUME e2fsck clean" || { echo "4KN RESUME e2fsck ISSUES"; rc=1; }

echo
echo "########## D: 分区身份/属性搬移后保持（4Kn 块设备）##########"
setup4k
set4kn "name keepname" name keepname
set4kn "flag esp on" flag esp on
echo "move 前现场：$(ls /var/lib/diskedit/ 2>/dev/null | wc -l) 项"
PU_BEFORE=$(sgdisk -i 2 "$LD" 2>/dev/null | grep -i "unique GUID" | awk '{print $NF}')
TY_BEFORE=$($B info "$LD" | grep -o '"num":2,[^}]*}' | grep -o '"type":"[^"]*"')
MOVE_OUT=$($B move "$LD":2 --start 66048 --chunk-size 4 2>&1); echo "move exit=$? : $(echo "$MOVE_OUT" | tail -1)"
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