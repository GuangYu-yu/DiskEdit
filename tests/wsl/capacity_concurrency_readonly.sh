#!/usr/bin/env bash
# 大容量（>2TiB 稀疏镜像，64-bit LBA）/ 只读设备拒绝 / 同镜像并发操作
source "$(dirname "$0")/lib.sh"
require_fault_bin

cleanup_hook() { rm -f /var/lib/diskedit/* 2>/dev/null; }

T=/var/tmp/t30.img
track_file "$T"

rm -f "$T" "$T".diskedit.* 2>/dev/null

echo "===== A: 3TiB GPT（稀疏镜像，秒级） ====="
rm -f "$T" "$T".diskedit.*
truncate -s 3T "$T"; ls -lh "$T" | awk '{print "sparse 占用:", $5}'
$B new "$T" --yes >/dev/null; echo "new exit=$?"
# 3TiB = 6442450944 扇区；last_usable = 6442450944 - 34 = 6442450910
$B add "$T" --start 2048 --end 3145727 --name p1 >/dev/null; echo "add p1 exit=$?"
$B add "$T" --start 6442000000 --end 6442450910 --name p2 >/dev/null; echo "add p2 @高LBA exit=$?"
$B resize-part "$T":2 --start 6442000000 --grow-to-end >/dev/null 2>&1; echo "p2 grow-to-end exit=$?"
$B info "$T" | grep -o '"num":2,"first_lba":[0-9]*,"last_lba":[0-9]*'
echo "  (期望 last_lba=6442450910)"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
# 尾部备份头落位验证：末扇区应为 GPT 头签名 "EFI PART"
tail -c 512 "$T" | head -c 8
echo " <- 期望 EFI PART"

echo
echo "===== B: 只读 loop → 写操作拒绝且不改盘 ====="
rm -f "$T" "$T".diskedit.*
truncate -s 64M "$T"
$B new "$T" --yes >/dev/null
# add 只建表项，不建文件系统（无 --fs 旗标）；格式化由下一行的 mkfs.ext4 显式完成
$B add "$T" --start 2048 --end 99999 --name p1 >/dev/null
LD=$(lo_attach "$T"); mkfs.ext4 -q "${LD}p1"; lo_detach "$LD"
BEFORE=$($B info "$T" | grep -o '"num":[^}]*}' | head -1)
MD_BEFORE=$(md5sum "$T" | cut -d' ' -f1)
LD=$(lo_attach "$T" -r); echo "ro loop: $LD"
$B info "$LD" >/dev/null 2>&1; echo "ro info exit=$? (期望 0)"
# 三个写探针都必须"在可写设备上合法、只因只读而被拒"，否则拒绝理由会来自别处：
#  - resize 用 56M（落在 last_usable 131038 之内）：若写 100M，拒绝理由会是"尺寸越界"(10)，
#    只读这件事根本没被测到
#  - 只读设备上写操作会先在 /var/lib 落下 journal 再 EPERM，两次探针之间须 abandon，
#    否则后一次会被"事务未释放"挡下，同样测不到只读
ro_probe() { # label cmd args...
  local label=$1 out e
  shift
  out=$("$@" 2>&1); e=$?
  echo "ro $label exit=$e (期望 30 EPERM)"
  if [ "$e" != "30" ]; then
    echo "  RO WRONG EXIT (BAD): $(echo "$out" | head -1)"; rc=1
  fi
  $B abandon "$LD" --yes >/dev/null 2>&1
}
ro_probe resize "$B" resize "$LD":1 56M
ro_probe set    "$B" set    "$LD":1 name ro-try
ro_probe delete "$B" delete "$LD":1 --yes
lo_detach "$LD"
[ "$MD_BEFORE" = "$(md5sum "$T" | cut -d' ' -f1)" ] && echo "RO DISK UNTOUCHED OK" || { echo "RO DISK CHANGED (BAD)"; rc=1; }
[ "$BEFORE" = "$($B info "$T" | grep -o '"num":[^}]*}' | head -1)" ] && echo "RO TABLE UNCHANGED OK" || { echo "RO TABLE CHANGED (BAD)"; rc=1; }

echo
echo "===== C: 同镜像并发 move ====="
rm -f "$T" "$T".diskedit.*
truncate -s 512M "$T"
$B new "$T" --yes >/dev/null
$B add "$T" --start 2048 --end 206847 --name p1 >/dev/null && $B mkfs "$T":1 ext4 --yes >/dev/null
$B add "$T" --start 411648 --end 616447 --name p2 >/dev/null && $B mkfs "$T":2 ext4 --yes >/dev/null
LD=$(lo_attach "$T")
if mount_at "${LD}p2" /testmnt; then
  dd if=/dev/urandom of=/testmnt/blob bs=1M count=90 2>/dev/null
  umount /testmnt 2>/dev/null
  MD=$(md5sum "${LD}p2" | cut -d' ' -f1)
else
  echo "MOUNT FAILED (BAD): ${LD}p2 -> /testmnt，并发基线缺失"; rc=1; MD=
fi
lo_detach "$LD"
# 两进程同时对 p2 自重叠左移；判定只看终态一致性（一个成功一个被拒/失败均为合法互斥结果）
DISKEDIT_FAULT=rs-chunk:70 $BF move "$T":2 --start 250000 --chunk-size 1 >/dev/null 2>&1 &
P1=$!
DISKEDIT_FAULT=rs-chunk:70 $BF move "$T":2 --start 250000 --chunk-size 1 >/dev/null 2>&1 &
P2=$!
wait $P1; E1=$?
wait $P2; E2=$?
echo "concurrent exits: $E1 / $E2"
echo "ckpt: $([ -f "$T.diskedit.ckpt" ] && echo present || echo absent)"
$B move "$T":2 --start 250000 --chunk-size 1 >/dev/null 2>&1; echo "settle rerun exit=$?"
LD=$(lo_attach "$T")
if [ -z "$MD" ]; then
  echo "CONCURRENCY DEV SKIP (基线 md5 缺失)"; rc=1
elif [ "$MD" = "$(md5sum "${LD}p2" | cut -d' ' -f1)" ]; then
  echo "CONCURRENCY DEV OK"
else
  echo "CONCURRENCY DEV CORRUPT"; rc=1
fi
e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "CONCURRENCY e2fsck clean" || { echo "CONCURRENCY e2fsck ISSUES"; rc=1; }
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc