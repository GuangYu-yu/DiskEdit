#!/usr/bin/env bash
# 补全中断窗口：after-repair（初始 ckpt 前）/ after-swap-commit（ckpt 推进前），
# 以及 ckpt 不可读时的硬拒（G）
source "$(dirname "$0")/lib.sh"
require_fault_bin

cleanup_hook() {
  local f
  for f in /var/tmp/diskedit_t17e.img /var/tmp/diskedit_t17f.img; do
    rm -f "$f" "$f"$SIDECAR_GLOB 2>/dev/null
  done
}

echo "===== E: after-repair（修复完成、初始 ckpt 写入前） ====="
T=/var/tmp/diskedit_t17e.img
rm -f "$T" "$T"$SIDECAR_GLOB
track_file "$T"
truncate -s 1G "$T"
$BF new "$T" --yes >/dev/null
$BF create "$T" --size 300M --name p1 >/dev/null
$BF create "$T" --size 300M --name p2 --fs ext4 >/dev/null
truncate -s 1200M "$T"   # 预扩容器 → backup GPT/PMBR 过期 → 触发 repair 路径
LD=$(lo_attach "$T")
mount_at "${LD}p2" /testmnt && dd if=/dev/urandom of=/testmnt/b2.bin bs=1M count=250 2>/dev/null; umount /testmnt; MD2=$(md5sum "${LD}p2" | cut -d' ' -f1); lo_detach "$LD"
# p1 是 grow 目标且没有 FS（E 段只有 p2 是 ext4）：扩分区表要显式 --no-fs
DISKEDIT_FAULT=after-repair $BF resize "$T":1 +100M --allow-move --yes --no-fs >/dev/null 2>&1
echo "aborted (expected); ckpt: $([ -f "$T$CKPT_SUFFIX" ] && echo yes || echo no)  journal: $([ -f "$T$JOURNAL_SUFFIX" ] && echo yes || echo no)"
# repair 已写表并越过不可逆边界（journal 落盘、ckpt 尚未建）：重跑被硬拒 30，
# 须先 undo/abandon 释放现场，不能当作"无现场"直接重算
OUT=$($BF resize "$T":1 +100M --allow-move --yes --no-fs 2>&1); E=$?
echo "rerun exit=$E : $(echo "$OUT" | head -1)"
exp "$E" 30 "repair 写入后重跑被拒（现场未释放）"
echo "$OUT" | grep -q "unfinished operation still owns this target" && echo "E OWNED-REPORT OK" || { echo "E report missing (BAD)"; rc=1; }
exp "$($B undo "$T" --yes >/dev/null 2>&1; echo $?)" 0 "undo 回滚 repair 写入"
$BF resize "$T":1 +100M --allow-move --yes --no-fs >/dev/null 2>&1; echo "rerun exit=$?"
LD=$(lo_attach "$T")
[ "$MD2" = "$(md5sum "${LD}p2" | cut -d' ' -f1)" ] && echo "p2 DATA OK" || { echo "p2 DATA CORRUPT"; rc=1; }
e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "p2 e2fsck clean" || { echo "p2 e2fsck ISSUES"; rc=1; }
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "===== F: after-swap-commit（GPT commit 后、ckpt 推进前，重放幂等） ====="
T=/var/tmp/diskedit_t17f.img
rm -f "$T" "$T"$SIDECAR_GLOB
track_file "$T"
truncate -s 1G "$T"
$BF new "$T" --yes >/dev/null
$BF create "$T" --size 300M --name p1 >/dev/null
$BF create "$T" --size 200M --name p2 --fs swap >/dev/null
$BF create "$T" --size 300M --name p3 --fs ext4 >/dev/null
LD=$(lo_attach "$T")
UUID_BEFORE=$(blkid -s UUID -o value "${LD}p2")
mount_at "${LD}p3" /testmnt && dd if=/dev/urandom of=/testmnt/b3.bin bs=1M count=250 2>/dev/null; umount /testmnt; MD3=$(md5sum "${LD}p3" | cut -d' ' -f1); lo_detach "$LD"
# p1 是 grow 目标且没有 FS：同样要 --no-fs（p2 的 swap 重建属搬移的一环，与本开关无关）
DISKEDIT_FAULT=after-swap-commit:1 $BF resize "$T":1 +100M --allow-move --yes --no-fs >/dev/null 2>&1
echo "aborted (expected)"
OUT=$($BF resize "$T":1 +100M --allow-move --yes --no-fs 2>&1); E=$?
echo "$OUT" | grep -o "resuming at entry [0-9]* chunk [0-9]*" | head -1
echo "rerun exit=$E"
LD=$(lo_attach "$T")
UUID_AFTER=$(blkid -s UUID -o value "${LD}p2")
[ "$MD3" = "$(md5sum "${LD}p3" | cut -d' ' -f1)" ] && echo "p3 DATA OK" || { echo "p3 DATA CORRUPT"; rc=1; }
e2fsck -fn "${LD}p3" >/dev/null 2>&1 && echo "p3 e2fsck clean" || { echo "p3 e2fsck ISSUES"; rc=1; }
lo_detach "$LD"
[ -n "$UUID_BEFORE" ] && [ "$UUID_BEFORE" = "$UUID_AFTER" ] && echo "SWAP UUID PRESERVED" || { echo "SWAP UUID MISMATCH"; rc=1; }
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "===== G: ckpt 不可读（头部损坏）→ 重跑硬拒 30 → abandon 释放 → 重跑 ====="
# 搬移目的地与源区不相交（右移越过源末尾）：中断后源数据完好，abandon 后全新重跑
# 才能得到正确结果。若用自重叠右移，中断已覆盖源尾部，abandon 后重跑只会读到坏源
T=/var/tmp/diskedit_t17g.img
rm -f "$T" "$T"$SIDECAR_GLOB
track_file "$T"
truncate -s 2G "$T"
$BF new "$T" --yes >/dev/null
$BF create "$T" --size 900M --name big --fs ext4 >/dev/null
LD=$(lo_attach "$T")
lo_waitpart "${LD}p1" || { echo "p1 part node not ready"; rc=1; }
mount_at "${LD}p1" /testmnt && dd if=/dev/urandom of=/testmnt/blob.bin bs=1M count=800 2>/dev/null
umount /testmnt
MD=$(md5sum "${LD}p1" | cut -d' ' -f1)
lo_detach "$LD"
# 2000896..3844095 = 2000896 起 1843200 扇区（900MiB，1MiB 对齐），与源区 2048..1845247 不相交
DISKEDIT_FAULT=rs-chunk:200 $BF resize-part "$T":1 --start 2000896 --end 3844095 --chunk-size 1 >/dev/null 2>&1
echo "aborted (expected); ckpt exists: $([ -f "$T$CKPT_SUFFIX" ] && echo yes || echo no)"
# 头部清零：不可读的现场必须硬拒（30）并指向 abandon，不能当作"没有现场"静默重跑
dd if=/dev/zero of="$T$CKPT_SUFFIX" bs=1 count=64 conv=notrunc 2>/dev/null
OUT=$($BF resize-part "$T":1 --start 2000896 --end 3844095 --chunk-size 1 2>&1); E=$?
echo "rerun exit=$E : $(echo "$OUT" | head -1)"
exp "$E" 30 "不可读 ckpt 时重跑被拒（不得静默重跑）"
echo "$OUT" | grep -q "none is readable" && echo "G UNREADABLE-CKPT REPORT OK" || { echo "G report missing (BAD)"; rc=1; }
exp "$($B abandon "$T" --yes >/dev/null 2>&1; echo $?)" 0 "abandon 释放不可读现场"
OUT=$($BF resize-part "$T":1 --start 104448 --end 1947647 --chunk-size 1 2>&1); exp "$?" 0 "abandon 后重跑完成"
LD=$(lo_attach "$T")
lo_waitpart "${LD}p1" || { echo "p1 part node not ready"; rc=1; }
assert_eq "$(md5sum "${LD}p1" | cut -d' ' -f1)" "$MD" "G 重跑后数据逐字节一致"
e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "p1 e2fsck clean" || { echo "p1 e2fsck ISSUES"; rc=1; }
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc