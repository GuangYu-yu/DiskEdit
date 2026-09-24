#!/usr/bin/env bash
# 补全最后两个中断窗口：after-repair（初始 ckpt 前）/ after-swap-commit（ckpt 推进前）
source "$(dirname "$0")/lib.sh"
require_fault_bin

cleanup_hook() {
  local f
  for f in /var/tmp/diskedit_t17e.img /var/tmp/diskedit_t17f.img; do
    rm -f "$f" "$f".diskedit.* 2>/dev/null
  done
}

echo "===== E: after-repair（修复完成、初始 ckpt 写入前） ====="
T=/var/tmp/diskedit_t17e.img
rm -f "$T" "$T".diskedit.*
track_file "$T"
truncate -s 1G "$T"
$BF new "$T" --yes >/dev/null
$BF create "$T" --size 300M --name p1 >/dev/null
$BF create "$T" --size 300M --name p2 --fs ext4 >/dev/null
truncate -s 1200M "$T"   # 预扩容器 → backup GPT/PMBR 过期 → 触发 repair 路径
LD=$(lo_attach "$T")
mount_at "${LD}p2" /testmnt && dd if=/dev/urandom of=/testmnt/b2.bin bs=1M count=250 2>/dev/null; umount /testmnt; MD2=$(md5sum "${LD}p2" | cut -d' ' -f1); lo_detach "$LD"
DISKEDIT_FAULT=after-repair $BF resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1
echo "aborted (expected); ckpt exists: $([ -f "$T.diskedit.ckpt" ] && echo yes || echo no)"
# 无 ckpt → 正常重算：repair 已收敛（再判为 Normal）→ 完整执行
$BF resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1; echo "rerun exit=$?"
LD=$(lo_attach "$T")
[ "$MD2" = "$(md5sum "${LD}p2" | cut -d' ' -f1)" ] && echo "p2 DATA OK" || { echo "p2 DATA CORRUPT"; rc=1; }
e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "p2 e2fsck clean" || { echo "p2 e2fsck ISSUES"; rc=1; }
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "===== F: after-swap-commit（GPT commit 后、ckpt 推进前，重放幂等） ====="
T=/var/tmp/diskedit_t17f.img
rm -f "$T" "$T".diskedit.*
track_file "$T"
truncate -s 1G "$T"
$BF new "$T" --yes >/dev/null
$BF create "$T" --size 300M --name p1 >/dev/null
$BF create "$T" --size 200M --name p2 --fs swap >/dev/null
$BF create "$T" --size 300M --name p3 --fs ext4 >/dev/null
LD=$(lo_attach "$T")
UUID_BEFORE=$(blkid -s UUID -o value "${LD}p2")
mount_at "${LD}p3" /testmnt && dd if=/dev/urandom of=/testmnt/b3.bin bs=1M count=250 2>/dev/null; umount /testmnt; MD3=$(md5sum "${LD}p3" | cut -d' ' -f1); lo_detach "$LD"
DISKEDIT_FAULT=after-swap-commit:1 $BF resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1
echo "aborted (expected)"
OUT=$($BF resize "$T":1 +100M --allow-move --yes 2>&1); E=$?
echo "$OUT" | grep -o "resuming at entry [0-9]* chunk [0-9]*" | head -1
echo "rerun exit=$E"
LD=$(lo_attach "$T")
UUID_AFTER=$(blkid -s UUID -o value "${LD}p2")
[ "$MD3" = "$(md5sum "${LD}p3" | cut -d' ' -f1)" ] && echo "p3 DATA OK" || { echo "p3 DATA CORRUPT"; rc=1; }
e2fsck -fn "${LD}p3" >/dev/null 2>&1 && echo "p3 e2fsck clean" || { echo "p3 e2fsck ISSUES"; rc=1; }
lo_detach "$LD"
[ -n "$UUID_BEFORE" ] && [ "$UUID_BEFORE" = "$UUID_AFTER" ] && echo "SWAP UUID PRESERVED" || { echo "SWAP UUID MISMATCH"; rc=1; }
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc