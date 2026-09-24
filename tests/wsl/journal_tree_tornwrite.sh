#!/usr/bin/env bash
# journal 流式改造 / FS 深度校验 / 复杂目录树 / torn write 模拟
# 残留控制：所有产物用 t22 前缀，trap EXIT 强制清理
source "$(dirname "$0")/lib.sh"
require_fault_bin

cleanup_hook() { rm -f /var/tmp/t22snap* /var/tmp/t22e2out /var/lib/diskedit/* 2>/dev/null; }

T=/var/tmp/t22a.img
SNAP=/var/tmp/t22snap
track_file "$T"

rm -f "$T" "$T"$SIDECAR_GLOB 2>/dev/null

mk() { # 100M/100M/200M，p2 前后留空便于自重叠左移
  rm -f "$T" "$T"$SIDECAR_GLOB
  truncate -s 1G "$T"
  $B new "$T" --yes >/dev/null
  $B add "$T" --start 2048 --end 206847 --name p1 >/dev/null && $B mkfs "$T":1 ext4 --yes >/dev/null
  $B add "$T" --start 411648 --end 616447 --name p2 >/dev/null && $B mkfs "$T":2 ext4 --yes >/dev/null
  $B add "$T" --start 1026048 --end 1435647 --name p3 >/dev/null && $B mkfs "$T":3 ext4 --yes >/dev/null
}
mnt() { LD=$(lo_attach "$T"); [ -n "$1" ] && { mount_at "${LD}p$1" /testmnt && return 0; }; true; }
umnt() { umount /testmnt 2>/dev/null; lo_detach "$LD"; }

echo "########## A: journal 流式（数据字节不入 journal） ##########"
mk
mnt 2; dd if=/dev/urandom of=/testmnt/blob bs=1M count=90 2>/dev/null; umnt; LD=$(lo_attach "$T"); MD2=$(md5sum "${LD}p2" | cut -d' ' -f1); lo_detach "$LD"
echo "-- A1: 中断的 100M 分区搬移后 journal 体积 --"
DISKEDIT_FAULT=rs-chunk:60 $BF move "$T":2 --start 250000 --chunk-size 1 >/dev/null 2>&1
JSZ=$(stat -c%s "$T$JOURNAL_SUFFIX" 2>/dev/null || echo 0)
echo "journal = $JSZ bytes (搬移量约 100M)"
if [ "$JSZ" -gt 5 ] && [ "$JSZ" -lt 1048576 ]; then echo "A1 JOURNAL-STREAMING OK (<1MiB, 含搬移标记)"; else { echo "A1 JOURNAL SIZE ODD ($JSZ)"; rc=1; }; fi
echo "-- A2: 含搬移的 undo 应被拒绝 --"
OUT=$($B undo "$T" --yes 2>&1); E=$?
echo "exit=$E msg: $(echo "$OUT" | head -1 | cut -c1-70)"
[ "$E" = "10" ] && echo "$OUT" | grep -q "data was moved" && echo "A2 UNDO-REFUSED OK" || { echo "A2 UNDO NOT REFUSED"; rc=1; }
echo "-- A3: 完成后 journal 应被删除 --"
$B move "$T":2 --start 250000 --chunk-size 1 >/dev/null 2>&1; E=$?
[ -f "$T$JOURNAL_SUFFIX" ] && { echo "A3 JOURNAL LEFT (bad)"; rc=1; } || echo "A3 JOURNAL DROPPED OK (exit=$E)"
LD=$(lo_attach "$T")
[ "$MD2" = "$(md5sum "${LD}p2" | cut -d' ' -f1)" ] && echo "A3 p2 DEV OK" || { echo "A3 p2 DEV CORRUPT"; rc=1; }
e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "A3 e2fsck clean" || { echo "A3 e2fsck ISSUES"; rc=1; }
lo_detach "$LD"

echo
echo "########## B: 复杂目录树 + 自重叠搬移 + e2fsck 深校验 ##########"
mk
mnt 2
mkdir -p /testmnt/a/b/c /testmnt/d
for i in $(seq 1 300); do echo "content-$i-abc" > "/testmnt/a/f$i.txt"; done
for i in $(seq 1 100); do echo "d-$i" > "/testmnt/d/g$i"; done
ln -s a/f1.txt /testmnt/link1
ln /testmnt/a/f2.txt /testmnt/hardlink1
dd if=/dev/urandom of=/testmnt/a/rand.bin bs=1M count=30 2>/dev/null
(cd /testmnt && find . -type f -exec md5sum {} \; | sort) > "$SNAP.1"
(cd /testmnt && find . | sort) > "$SNAP.t1"
echo "files: $(wc -l < "$SNAP.1"), symlink: $(readlink /testmnt/link1), hardlink count: $(stat -c%h /testmnt/a/f2.txt)"
umnt
# 自重叠左移：delta = 249856-411648 = -161792，|delta| < 长度 204800
$B move "$T":2 --start 250000 --chunk-size 1 >/dev/null 2>&1; echo "move exit=$?"
LD=$(lo_attach "$T")
e2fsck -fn "${LD}p2" > /var/tmp/t22e2out 2>&1; EC=$?
echo "e2fsck -fn exit=$EC (0=clean)"
tail -2 /var/tmp/t22e2out
mount_at "${LD}p2" /testmnt
(cd /testmnt && find . -type f -exec md5sum {} \; | sort) > "$SNAP.2"
(cd /testmnt && find . | sort) > "$SNAP.t2"
diff -q "$SNAP.1" "$SNAP.2" >/dev/null && echo "B FILE-CONTENTS OK (400+ files)" || { echo "B FILE CONTENTS DIFFER"; rc=1; diff "$SNAP.1" "$SNAP.2" | head -5; }
diff -q "$SNAP.t1" "$SNAP.t2" >/dev/null && echo "B TREE OK" || { echo "B TREE DIFFERS"; rc=1; }
[ "$(readlink /testmnt/link1)" = "a/f1.txt" ] && echo "B SYMLINK OK" || { echo "B SYMLINK BROKEN"; rc=1; }
[ "$(stat -c%h /testmnt/a/f2.txt)" -ge 2 ] && echo "B HARDLINK OK" || { echo "B HARDLINK BROKEN"; rc=1; }
umnt

echo
echo "########## C: torn write 模拟（GPT 头损坏 / 备份头损坏） ##########"
mk
echo "-- C1: 擦除主头（LBA1）后读表 --"
dd if=/dev/zero of="$T" bs=512 seek=1 count=1 conv=notrunc 2>/dev/null
OUT=$($B info "$T" 2>&1)
if echo "$OUT" | grep -q '"label":"none"'; then
  echo "C1 GAP: 主头损坏 → 工具视为无表（不回退备份头）"
else
  echo "C1 OK: 从备份头读到表"
fi
echo "-- C2: 主头损坏时写操作（工具已从备份头恢复表 → 写后应自洽）--"
$B resize-part "$T":3 --start 1026048 --grow-to-end >/dev/null 2>&1; E=$?
echo "write exit=$E"
sgdisk -v "$T" >/dev/null 2>&1 && echo "C2 RECOVERED-CLEAN OK" || { echo "C2 INCONSISTENT AFTER WRITE"; rc=1; }
echo "-- C3: 主头损坏 + new → 备份头被覆盖？（数据丢失风险） --"
$B new "$T" --yes >/dev/null 2>&1; echo "new exit=$?"
N=$($B info "$T" | grep -o '"num":' | wc -l)
echo "new 后可见分区数: $N"
sgdisk -v "$T" 2>&1 | tail -1
echo "-- C4: 擦除备份头（盘尾）→ 主头可用则不受影响 --"
mk
SZ=$(stat -c%s "$T"); LAST=$((SZ / 512 - 1))
dd if=/dev/zero of="$T" bs=512 seek=$LAST count=1 conv=notrunc 2>/dev/null
echo "info: $($B info "$T" | head -c 50)"
$B resize-part "$T":3 --start 1026048 --grow-to-end >/dev/null 2>&1; echo "repair exit=$?"
$B info "$T" | grep -o '"num":3,[^}]*}'
sgdisk -v "$T" 2>&1 | tail -1

echo "-- C5: 保护 MBR SizeInLBA 超出容器（Inconsistent）→ 拒绝自动修复 --"
# 依据：src/table.rs:1071-1075 pmbr_size_state：size > 容器-1（两种口径都大于）→ Inconsistent；
# src/gpt_policy.rs:88-94 classify_repair 对 Inconsistent 返回 Err（拒绝自动修复，可能是更大盘的
# 截断副本），经 src/gpt_policy.rs:174 归 Fail::infra → 退出码 30（未写盘的环境/盘内容故障）；
# info 走 load_gpt（不做修复）仍可读，并把提示打入 stderr（src/cmd/info.rs:65-67）
mk
printf '\xff\xff\xff\xff' | dd of="$T" bs=1 seek=458 conv=notrunc 2>/dev/null
OUT=$($B info "$T" 2>&1); E=$?
echo "info exit=$E : $(echo "$OUT" | grep -o 'refusing auto-repair[^"]*' | head -1)"
if [ "$E" = "0" ] && echo "$OUT" | grep -q "refusing auto-repair"; then
  echo "C5 INFO-REPORTS OK"
else
  echo "C5 INFO WRONG (exit=$E)"; rc=1
fi
OUT=$($B set "$T":1 flag esp on 2>&1); E=$?
echo "write exit=$E : $(echo "$OUT" | tail -1 | cut -c1-90)"
if [ "$E" = "30" ] && echo "$OUT" | grep -q "refusing auto-repair"; then
  echo "C5 INCONSISTENT-REFUSED OK"
else
  echo "C5 WRONG EXIT (exit=$E, want 30)"; rc=1
fi

echo
echo "########## 残留核对 ##########"
ls /var/tmp/t22* 2>/dev/null | wc -l
ls /var/lib/diskedit/ 2>/dev/null | wc -l
losetup -a | wc -l
exit $rc