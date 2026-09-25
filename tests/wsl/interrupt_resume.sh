#!/usr/bin/env bash
# 中断后恢复 v3
source "$(dirname "$0")/lib.sh"
require_bin

T=/var/tmp/diskedit_test13.img

cleanup_hook() { rm -f /var/tmp/diskedit_test13*.img* 2>/dev/null; }

rm -f /var/tmp/diskedit_test13*.img* 2>/dev/null
track_file "$T"

rm -f "$T" "$T"$SIDECAR_GLOB
truncate -s 2G "$T"
$B new "$T" --yes || exit 1
$B create "$T" --size 900M --name big --fs ext4

LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt
dd if=/dev/urandom of=/testmnt/blob.bin bs=1M count=800 2>/dev/null
umount /testmnt
MD_BEFORE=$(md5sum "${LD}p1" | cut -d' ' -f1)
echo "md5 before: $MD_BEFORE"
lo_detach "$LD"

echo "== 900M 分区右移 50M（chunk 1MiB），1 秒后 kill -9 =="
$B resize-part "$T":1 --start 104448 --end 1947647 --chunk-size 1 &
PID=$!
sleep 1
kill -9 $PID 2>/dev/null; wait $PID 2>/dev/null
[ -f "$T$CKPT_SUFFIX" ] && echo "checkpoint exists: yes" || echo "checkpoint exists: NO (may have finished)"

echo "== 重跑同一命令 → 续传 =="
$B resize-part "$T":1 --start 104448 --end 1947647 --chunk-size 1; echo "exit=$?"

echo "== 完整性校验 =="
LD=$(lo_attach "$T")
MD_AFTER=$(md5sum "${LD}p1" | cut -d' ' -f1)
echo "md5 after:  $MD_AFTER"
[ -n "$MD_BEFORE" ] && [ "$MD_BEFORE" = "$MD_AFTER" ] && echo "RESUME DATA OK" || { echo "RESUME DATA CORRUPT"; rc=1; }
e2fsck -fn "${LD}p1" >/dev/null 2>&1; echo "e2fsck exit=$?"
lo_detach "$LD"
sgdisk -v "$T" | tail -2

echo "== shift 路径中断恢复 =="
T2=/var/tmp/diskedit_test13b.img
rm -f "$T2" "$T2"$SIDECAR_GLOB
truncate -s 1G "$T2"
$B new "$T2" --yes
$B create "$T2" --size 500M --name a
$B create "$T2" --size 400M --name b --fs ext4
LD=$(lo_attach "$T2")
mount_at "${LD}p2" /testmnt
dd if=/dev/urandom of=/testmnt/blob2.bin bs=1M count=350 2>/dev/null
umount /testmnt
MD2=$(md5sum "${LD}p2" | cut -d' ' -f1)
echo "md5 b before: $MD2"
lo_detach "$LD"
# a 是 grow 目标且没有 FS：扩分区表要显式 --no-fs（本段验的是中断后续跑）
$B resize "$T2":1 +100M --allow-move --yes --chunk-size 1 --no-fs &
PID=$!
sleep 0.5
kill -9 $PID 2>/dev/null; wait $PID 2>/dev/null
[ -f "$T2$CKPT_SUFFIX" ] && echo "checkpoint exists: yes" || echo "checkpoint exists: NO (may have finished)"
$B resize "$T2":1 +100M --allow-move --yes --chunk-size 1 --no-fs; echo "resume exit=$?"
LD=$(lo_attach "$T2")
MD2_AFTER=$(md5sum "${LD}p2" | cut -d' ' -f1)
echo "md5 b after:  $MD2_AFTER"
[ -n "$MD2" ] && [ "$MD2" = "$MD2_AFTER" ] && echo "SHIFT-RESUME DATA OK" || { echo "SHIFT-RESUME DATA CORRUPT"; rc=1; }
e2fsck -fn "${LD}p2" >/dev/null 2>&1; echo "e2fsck exit=$?"
lo_detach "$LD"
sgdisk -v "$T2" | tail -2
exit $rc