#!/usr/bin/env bash
# 边界的成对断言（批 6）：同一条事务里
#   "写盘前 abort" 必须与「从未运行过」不可区分（表未改、无 checkpoint 残留）
#   "写盘后 abort" 必须在盘上留下可续传的现场（checkpoint 已落盘）
# 三条流程各成一对：resize(离线含搬移) / apply / resize(在线)。
# 注入开关默认关闭（--features test-faults），普通构建与发布二进制行为不变
source "$(dirname "$0")/lib.sh"
require_fault_bin

T=/var/tmp/tcb.img
CK="$T$CKPT_SUFFIX"
MNT=/testmnt
ok=0; bad=0

chk() { # got want what
  if [ "$1" = "$2" ]; then echo "  OK   $3"; ok=$((ok+1)); else echo "  BAD  $3 (got [$1] want [$2])"; bad=$((bad+1)); fi
}

rm -f "$T" "$T"$SIDECAR_GLOB 2>/dev/null
mkdir -p "$MNT"
track_file "$T"

mk() { # p1 扩容需要把 p2/p3 往后搬，故必有搬移相位
  rm -f "$T" "$T"$SIDECAR_GLOB
  truncate -s 1G "$T"
  $BF new "$T" --yes >/dev/null
  $BF create "$T" --size 100M --name p1 --fs ext4 >/dev/null
  $BF create "$T" --size 100M --name p2 --fs ext4 >/dev/null
  $BF create "$T" --size 200M --name p3 --fs ext4 >/dev/null
}

echo "===== A: resize（离线，含搬移）====="
mk
BEFORE=$($B info "$T")
DISKEDIT_FAULT=before-any-write $BF resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1; A1=$?
chk "$A1" "134" "A1 边界前 abort（134 = SIGABRT）"
chk "$($B info "$T")" "$BEFORE" "A1 分区表一字未改"
chk "$([ -e "$CK" ] && echo yes || echo no)" "no" "A1 未留下 checkpoint"

DISKEDIT_FAULT=chunk:40 $BF resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1; A2=$?
chk "$A2" "134" "A2 边界后 abort"
chk "$([ -e "$CK" ] && echo yes || echo no)" "yes" "A2 留下 checkpoint（续传现场）"
$B resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1; chk "$?" "0" "A2 重跑收敛（exit 0）"
LD=$(lo_attach "$T"); e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "  OK   A2 e2fsck clean" || { echo "  BAD  A2 e2fsck ISSUES"; bad=$((bad+1)); }; lo_detach "$LD"

echo
echo "===== B: apply ====="
mk
BEFORE=$($B info "$T")
$B plan "$T" --grow 1 | head -3 | sed 's/^/  plan | /'
# apply 无 --yes（执行的是 plan 已打印的那份计划，没有第二个确认层），多传即 10
DISKEDIT_FAULT=before-any-write $BF apply "$T" --grow 1 >/dev/null 2>&1; B1=$?
chk "$B1" "134" "B1 边界前 abort"
chk "$($B info "$T")" "$BEFORE" "B1 分区表一字未改"
chk "$([ -e "$CK" ] && echo yes || echo no)" "no" "B1 未留下 checkpoint"

DISKEDIT_FAULT=chunk:1 $BF apply "$T" --grow 1 >/dev/null 2>&1; B2=$?
chk "$B2" "134" "B2 边界后 abort"
chk "$([ -e "$CK" ] && echo yes || echo no)" "yes" "B2 留下 checkpoint（续传现场）"
$B apply "$T" --grow 1 >/dev/null 2>&1; chk "$?" "0" "B2 重跑收敛（exit 0）"

echo
echo "===== C: resize（在线，sfdisk 分界）====="
rm -f "$T" "$T"$SIDECAR_GLOB
truncate -s 64M "$T"
$BF new "$T" --yes >/dev/null
$BF create "$T" --size 16777216 --name p1 --fs ext4 >/dev/null
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$MNT" || { echo "  BAD  C MOUNT-FAIL"; exit 1; }
DISKEDIT_FAULT=online-before-write $BF resizefs "$MNT" 33554432 --online >/dev/null 2>&1; C1=$?
umount "$MNT"
chk "$C1" "134" "C1 sfdisk 前 abort"
# p1 的末端 LBA：16MiB = 32768 扇区 ⇒ 2048+32768-1
chk "$($B info "$T" | grep -o '"last_lba":[0-9]*' | head -1)" '"last_lba":34815' "C1 分区仍 16MiB（表未改）"

mount_at "${LD}p1" "$MNT" || { echo "  BAD  C MOUNT-FAIL(2)"; exit 1; }
DISKEDIT_FAULT=online-after-write $BF resizefs "$MNT" 33554432 --online >/dev/null 2>&1; C2=$?
umount "$MNT"; lo_detach "$LD"
chk "$C2" "134" "C2 sfdisk 后 abort"
# 32MiB = 65536 扇区 ⇒ 2048+65536-1
chk "$($B info "$T" | grep -o '"last_lba":[0-9]*' | head -1)" '"last_lba":67583' "C2 分区已 32MiB（表已改）"

echo
echo "===== D: 天然拒绝仍必须报 10（未写盘的承诺）====="
mk
BEFORE=$($B info "$T")
$B resize "$T":1 -50M --no-fs >/dev/null 2>&1; chk "$?" "10" "D1 --no-fs + 缩容 ⇒ 10"
$B resize "$T":1 500M --start 4096 >/dev/null 2>&1; chk "$?" "10" "D2 --start ⇒ 10（resize 不搬移）"
$B resize-part "$T":1 --start end >/dev/null 2>&1; chk "$?" "10" "D3 resize-part --start end ⇒ 10"
chk "$($B info "$T")" "$BEFORE" "D 三次拒绝均未写盘"

echo
echo "==== 汇总：$ok 通过，$bad 失败 ===="
[ "$bad" -eq 0 ]