#!/usr/bin/env bash
# 边界的成对断言：同一条事务里
#   "写盘前 abort" 必须与「从未运行过」不可区分（表未改、无记录残留）
#   "写盘后 abort" 必须在盘上留下可续传的现场（记录已落盘）
# 三条流程各成一对：resize(离线含搬移) / resize grow / resize(在线)。
source "$(dirname "$0")/lib.sh"
require_fault_bin

img T tcb.img
MNT="$TEST_MNT"
ok=0; bad=0

chk() { # got want what
  if [ "$1" = "$2" ]; then echo "  OK   $3"; ok=$((ok+1)); else echo "  BAD  $3 (got [$1] want [$2])"; bad=$((bad+1)); fi
}

rm -rf "$T" "$T"$SIDECAR_GLOB 2>/dev/null
mkdir -p "$MNT"

mk() { # p1 扩容需要把 p2/p3 往后搬
  rm -rf "$T" "$T"$SIDECAR_GLOB
  truncate -s 1G "$T"
  $BF new "$T" --yes >/dev/null
  $BF add "$T" 100M --align mib --name p1 --yes >/dev/null
  $BF mkfs "$T":1 ext4 --yes >/dev/null
  $BF add "$T" 100M --align mib --name p2 --yes >/dev/null
  $BF mkfs "$T":2 ext4 --yes >/dev/null
  $BF add "$T" 200M --align mib --name p3 --yes >/dev/null
  $BF mkfs "$T":3 ext4 --yes >/dev/null
}

echo "===== A: resize（离线，含搬移）====="
mk
BEFORE=$($B info "$T")
DISKEDIT_FAULT=before-any-write $BF resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1; A1=$?
chk "$A1" "134" "A1 边界前 abort（134 = SIGABRT）"
chk "$($B info "$T")" "$BEFORE" "A1 分区表一字未改"
chk "$(record_present "$T" && echo yes || echo no)" "no" "A1 未留下记录"

DISKEDIT_FAULT=chunk:40 $BF resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1; A2=$?
chk "$A2" "134" "A2 边界后 abort"
chk "$(record_present "$T" && echo yes || echo no)" "yes" "A2 留下记录（续传现场）"
OUT=$($B resize "$T":1 +100M --allow-move --yes 2>&1); A2R=$?
echo "  rerun exit=$A2R"; echo "$OUT" | head -3 | sed "s/^/    /"
# 中断点在搬移相位的块边界：记录停在"搬到一半"，表还没提交。续跑从该块接着搬，
# 再走完表提交与 FS 扩容——收敛的判据是这条请求整体完成（0）。这里若期望 5，
# 等于把"续跑被自己的记录挡住"当成正常收尾
chk "$A2R" "0" "A2 重跑收敛（续跑就地走完）"
chk "$(echo "$OUT" | grep -c 'resuming at entry')" "1" "A2 续跑走记录（resuming at entry）"
chk "$($B info "$T" | grep -o '"last_lba":[0-9]*' | head -1)" '"last_lba":411647' "A2 扩容已落到表"
chk "$(record_present "$T" && echo yes || echo no)" "no" "A2 完成后无记录残留"
LD=$(lo_attach "$T")
for n in 1 2 3; do
  e2fsck -fn "${LD}p$n" >/dev/null 2>&1 && echo "  OK   A2 p$n e2fsck clean" || { echo "  BAD  A2 p$n e2fsck ISSUES"; bad=$((bad+1)); }
done
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "  OK   A2 sgdisk clean" || { echo "  BAD  A2 sgdisk ISSUES"; bad=$((bad+1)); }

echo
echo "===== B: resize grow --allow-move ====="
mk
BEFORE=$($B info "$T")
$B plan "$T":1 --grow | head -3 | sed 's/^/  plan | /'
# resize grow 与搬移联动走同一条几何路径（resize --allow-move 一次生成多条目计划）：本布局的 p2/p3 需要搬移，故必须显式 --yes
DISKEDIT_FAULT=before-any-write $BF resize "$T":1 grow --allow-move --yes >/dev/null 2>&1; B1=$?
chk "$B1" "134" "B1 边界前 abort"
chk "$($B info "$T")" "$BEFORE" "B1 分区表一字未改"
chk "$(record_present "$T" && echo yes || echo no)" "no" "B1 未留下记录"

DISKEDIT_FAULT=chunk:1 $BF resize "$T":1 grow --allow-move --yes >/dev/null 2>&1; B2=$?
chk "$B2" "134" "B2 边界后 abort"
chk "$(record_present "$T" && echo yes || echo no)" "yes" "B2 留下记录（续传现场）"
OUT=$($B resize "$T":1 grow --allow-move --yes 2>&1); B2R=$?
echo "  rerun exit=$B2R"; echo "$OUT" | head -3 | sed "s/^/    /"
# 与 A2 同一判据：中断在第一个块之后，续跑必须从记录接住并走完整条请求
chk "$B2R" "0" "B2 重跑收敛（续跑就地走完）"
chk "$(echo "$OUT" | grep -c 'resuming at entry')" "1" "B2 续跑走记录（resuming at entry）"
chk "$($B info "$T" | grep -o '"last_lba":[0-9]*' | head -1)" '"last_lba":1482718' "B2 扩容已落到表"
chk "$(record_present "$T" && echo yes || echo no)" "no" "B2 完成后无记录残留"
LD=$(lo_attach "$T")
for n in 1 2 3; do
  e2fsck -fn "${LD}p$n" >/dev/null 2>&1 && echo "  OK   B2 p$n e2fsck clean" || { echo "  BAD  B2 p$n e2fsck ISSUES"; bad=$((bad+1)); }
done
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "  OK   B2 sgdisk clean" || { echo "  BAD  B2 sgdisk ISSUES"; bad=$((bad+1)); }

echo
echo "===== C: resize（在线，sfdisk 分界）====="
rm -rf "$T" "$T"$SIDECAR_GLOB
truncate -s 64M "$T"
$BF new "$T" --yes >/dev/null
$BF add "$T" 16777216 --align mib --name p1 --yes >/dev/null
$BF mkfs "$T":1 ext4 --yes >/dev/null
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$MNT" || { echo "  BAD  C MOUNT-FAIL"; exit 1; }
DISKEDIT_FAULT=online-before-write $BF resize --yes "${LD}:1" 33554432 >/dev/null 2>&1; C1=$?
umount "$MNT"
chk "$C1" "134" "C1 sfdisk 前 abort"
# p1 的末端 LBA：16MiB = 32768 扇区 ⇒ 2048+32768-1
chk "$($B info "$T" | grep -o '"last_lba":[0-9]*' | head -1)" '"last_lba":34815' "C1 分区仍 16MiB（表未改）"

mount_at "${LD}p1" "$MNT" || { echo "  BAD  C MOUNT-FAIL(2)"; exit 1; }
DISKEDIT_FAULT=online-after-write $BF resize --yes "${LD}:1" 33554432 >/dev/null 2>&1; C2=$?
umount "$MNT"; lo_detach "$LD"
chk "$C2" "134" "C2 sfdisk 后 abort"
# 32MiB = 65536 扇区 ⇒ 2048+65536-1
chk "$($B info "$T" | grep -o '"last_lba":[0-9]*' | head -1)" '"last_lba":67583' "C2 分区已 32MiB（表已改）"

echo
echo "===== D: 天然拒绝仍必须报 3/2（未写盘的承诺）====="
mk
BEFORE=$($B info "$T")
$B resize "$T":1 -50M --no-fs --yes >/dev/null 2>&1; chk "$?" "3" "D1 --no-fs + 缩容 ⇒ 3"
$B resize --yes "$T":1 500M --start 4096 >/dev/null 2>&1; chk "$?" "2" "D2 --start 非本命令选项（解析期即拒，未写盘）" # cli-surface: refused
$B resize-part --yes "$T":1 --start end >/dev/null 2>&1; chk "$?" "2" "D3 resize-part --start end ⇒ 2" # cli-surface: refused
chk "$($B info "$T")" "$BEFORE" "D 三次拒绝均未写盘"

echo
echo "==== 汇总：$ok 通过，$bad 失败 ===="
[ "$bad" -eq 0 ]