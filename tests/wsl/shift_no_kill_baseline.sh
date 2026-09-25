#!/usr/bin/env bash
# 隔离测试：不中断的 shift apply 是否损坏数据
source "$(dirname "$0")/lib.sh"
require_bin

T=/var/tmp/diskedit_test14.img

track_file "$T"

rm -f "$T" "$T"$SIDECAR_GLOB
truncate -s 1G "$T"
$B new "$T" --yes
$B create "$T" --size 500M --name a
$B create "$T" --size 400M --name b --fs ext4
LD=$(lo_attach "$T")
mount_at "${LD}p2" /testmnt
dd if=/dev/urandom of=/testmnt/blob2.bin bs=1M count=350 2>/dev/null
umount /testmnt
MD=$(md5sum "${LD}p2" | cut -d' ' -f1)
echo "md5 before: $MD"
lo_detach "$LD"

# a 是 grow 目标且没有 FS：扩分区表要显式 --no-fs（本段只验最小位移搬移不动 b 的数据）
$B resize "$T":1 +100M --allow-move --yes --chunk-size 1 --no-fs; echo "exit=$?"

LD=$(lo_attach "$T")
MD2=$(md5sum "${LD}p2" | cut -d' ' -f1)
echo "md5 after:  $MD2"
[ "$MD" = "$MD2" ] && echo "NO-KILL SHIFT DATA OK" || { echo "NO-KILL SHIFT DATA CORRUPT"; rc=1; }
umount /testmnt 2>/dev/null
e2fsck -f -y "${LD}p2" >/dev/null 2>&1; echo "e2fsck exit=$?"
lo_detach "$LD"
exit $rc