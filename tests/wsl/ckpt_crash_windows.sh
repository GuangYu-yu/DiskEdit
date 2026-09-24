#!/usr/bin/env bash
# test-faults 补全：跨条目间隙 / 连续多次中断 / before-grow 完整重放 / swap 分支 / 自重叠
source "$(dirname "$0")/lib.sh"
require_fault_bin

mk_layout() { # $1=img $2=extra create args...
  local T=$1; shift
  rm -f "$T" "$T".diskedit.*
  truncate -s 2G "$T"
  $BF new "$T" --yes >/dev/null
  $BF create "$T" --size 500M --name p1 >/dev/null
  $BF create "$T" --size 400M --name p2 --fs ext4 >/dev/null
  $BF create "$T" --size 400M --name p3 --fs ext4 >/dev/null
}

fill_p23() { # 随机数据填 p2/p3，记录整分区哈希
  local T=$1
  LD=$(lo_attach "$T")
  mount_at "${LD}p2" /testmnt && dd if=/dev/urandom of=/testmnt/b2.bin bs=1M count=350 2>/dev/null
  umount /testmnt
  mount_at "${LD}p3" /testmnt && dd if=/dev/urandom of=/testmnt/b3.bin bs=1M count=350 2>/dev/null
  umount /testmnt
  MD2=$(md5sum "${LD}p2" | cut -d' ' -f1)
  MD3=$(md5sum "${LD}p3" | cut -d' ' -f1)
  losetup -d "$LD"
}

check_data() { # $1=img
  local T=$1 rc=0
  LD=$(lo_attach "$T")
  MD2G=$(md5sum "${LD}p2" | cut -d' ' -f1)
  [ "$MD2" = "$MD2G" ] && echo "p2 DATA OK" || { echo "p2 DATA CORRUPT"; rc=1; }
  MD3G=$(md5sum "${LD}p3" | cut -d' ' -f1)
  [ "$MD3" = "$MD3G" ] && echo "p3 DATA OK" || { echo "p3 DATA CORRUPT"; rc=1; }
  e2fsck -fn "${LD}p2" >/dev/null 2>&1 || { echo "p2 e2fsck FAILED"; rc=1; }
  e2fsck -fn "${LD}p3" >/dev/null 2>&1 || { echo "p3 e2fsck FAILED"; rc=1; }
  losetup -d "$LD"
  sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
  return $rc
}

T=/var/tmp/diskedit_t16a.img
track_file "$T"

echo "===== A: 跨条目间隙 + 连续多次中断 ====="
mk_layout "$T"; fill_p23 "$T"
# 轮1：entry 0（p3）commit 后死
DISKEDIT_FAULT=after-entry-commit:0 $BF resize "$T":1 +200M --allow-move --yes >/dev/null 2>&1
echo "round1 aborted (expected)"
# 轮2：entry 1（p2，100 chunks）拷到第 70 chunk 死 → durable=64
OUT=$(DISKEDIT_FAULT=chunk:70 $BF resize "$T":1 +200M --allow-move --yes 2>&1)
echo "$OUT" | grep -o "resuming at entry [0-9]* chunk [0-9]*" | head -1
echo "round2 aborted (expected)"
# 轮3：无注入完成 → resume at entry 1 chunk 64
OUT=$($BF resize "$T":1 +200M --allow-move --yes 2>&1)
R=$(echo "$OUT" | grep -o "resuming at entry [0-9]* chunk [0-9]*" | head -1)
echo "round3: $R (want entry 1 chunk 64)"
[ "$R" = "resuming at entry 1 chunk 64" ] && echo "RESUME-CHAIN OK" || { echo "RESUME-CHAIN WRONG"; rc=1; }
check_data "$T" || rc=1
# p1 终态 = 500M + 200M = 700M
P1=$($B info "$T" | grep -o '"num":1,[^}]*}' | grep -o '"size_bytes":[0-9]*' | cut -d: -f2)
echo "p1 size_bytes: $P1 (期望 700M = 734003200)"
echo

echo "===== B: before-grow → 间隙已够，重跑经普通路径收敛 ====="
mk_layout "$T"; fill_p23 "$T"
DISKEDIT_FAULT=before-grow $BF resize "$T":1 +200M --allow-move --yes >/dev/null 2>&1
echo "aborted (expected)"
# p2/p3 已搬完、p1 未扩：free_right 恰好 = shift → 普通扩容路径直接收敛（合法）
$BF resize "$T":1 +200M --allow-move --yes >/dev/null 2>&1; echo "round2 exit=$?"
check_data "$T" || rc=1
echo "p1 终态几何（应为 700M = 500M+200M）："
$B info "$T"

T=/var/tmp/diskedit_t16c.img
track_file "$T"
echo "===== C: swap 挡路分支 + after-swap-entry ====="
rm -f "$T" "$T".diskedit.*
truncate -s 1G "$T"
$BF new "$T" --yes >/dev/null
$BF create "$T" --size 300M --name p1 >/dev/null
$BF create "$T" --size 200M --name p2 --fs swap >/dev/null
$BF create "$T" --size 300M --name p3 --fs ext4 >/dev/null
LD=$(lo_attach "$T")
mount_at "${LD}p3" /testmnt && dd if=/dev/urandom of=/testmnt/b3.bin bs=1M count=250 2>/dev/null
umount /testmnt
MD3=$(md5sum "${LD}p3" | cut -d' ' -f1)
UUID_BEFORE=$(blkid -s UUID -o value "${LD}p2")
losetup -d "$LD"
echo "swap uuid before: $UUID_BEFORE"
DISKEDIT_FAULT=after-swap-entry:1 $BF resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1
echo "aborted (expected)"
# swap 已落位重建、p3 已搬完：free_right = shift → 普通路径收敛
$BF resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1; echo "round2 exit=$?"
LD=$(lo_attach "$T")
UUID_AFTER=$(blkid -s UUID -o value "${LD}p2")
MD3G=$(md5sum "${LD}p3" | cut -d' ' -f1)
[ "$MD3" = "$MD3G" ] && echo "p3 DATA OK" || { echo "p3 DATA CORRUPT"; rc=1; }
losetup -d "$LD"
[ -n "$UUID_BEFORE" ] && [ "$UUID_BEFORE" = "$UUID_AFTER" ] && echo "SWAP UUID PRESERVED" || { echo "SWAP UUID MISMATCH ($UUID_BEFORE -> $UUID_AFTER)"; rc=1; }

T=/var/tmp/diskedit_t16d.img
track_file "$T"
echo
echo "===== D: 自重叠搬移（delta 100M < len 300M）+ chunk 注入 ====="
rm -f "$T" "$T".diskedit.*
truncate -s 1G "$T"
$BF new "$T" --yes >/dev/null
$BF create "$T" --size 400M --name p1 >/dev/null
$BF create "$T" --size 300M --name p2 --fs ext4 >/dev/null
LD=$(lo_attach "$T")
mount_at "${LD}p2" /testmnt && dd if=/dev/urandom of=/testmnt/b2.bin bs=1M count=250 2>/dev/null
umount /testmnt
MD2=$(md5sum "${LD}p2" | cut -d' ' -f1)
losetup -d "$LD"
DISKEDIT_FAULT=chunk:33 $BF resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1
echo "aborted (expected)"
OUT=$($BF resize "$T":1 +100M --allow-move --yes 2>&1)
R=$(echo "$OUT" | grep -o "resuming at entry [0-9]* chunk [0-9]*" | head -1)
echo "resume: $R (want entry 0 chunk 32)"
[ "$R" = "resuming at entry 0 chunk 32" ] && echo "SELF-OVERLAP RESUME OK" || { echo "SELF-OVERLAP RESUME WRONG"; rc=1; }
LD=$(lo_attach "$T")
MD2G=$(md5sum "${LD}p2" | cut -d' ' -f1)
[ "$MD2" = "$MD2G" ] && echo "p2 DATA OK" || { echo "p2 DATA CORRUPT"; rc=1; }
e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "e2fsck clean" || { echo "e2fsck FAILED"; rc=1; }
losetup -d "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc