#!/usr/bin/env bash
# 隔离测试：不中断的最小位移搬移（resize SIZE --allow-move）是否损坏数据
source "$(dirname "$0")/lib.sh"
require_bin

img T baseline14.img


rm -rf "$T" "$T"$SIDECAR_GLOB
truncate -s 1G "$T"
$B new "$T" --yes
$B add "$T" 500M --align mib --name a --yes
$B add "$T" 400M --align mib --name b --yes
$B mkfs "$T":2 ext4 --yes
LD=$(lo_attach "$T")
mount_at "${LD}p2" "$TEST_MNT"
dd if=/dev/urandom of="$TEST_MNT/blob2.bin" bs=1M count=350 2>/dev/null
umount "$TEST_MNT"
MD=$(mdsum "${LD}p2")
echo "md5 before: $MD"
lo_detach "$LD"

# a 是 grow 目标且没有 FS：扩分区表要显式 --no-fs（本段只验最小位移搬移不动 b 的数据）
$B resize "$T":1 +100M --allow-move --yes --chunk 1 --no-fs; echo "exit=$?"

LD=$(lo_attach "$T")
MD2=$(mdsum "${LD}p2")
echo "md5 after:  $MD2"
[ "$MD" = "$MD2" ] && echo "NO-KILL SHIFT DATA OK" || { echo "NO-KILL SHIFT DATA CORRUPT"; rc=1; }
umount "$TEST_MNT" 2>/dev/null
e2fsck -f -y "${LD}p2" >/dev/null 2>&1; echo "e2fsck exit=$?"
lo_detach "$LD"
exit $rc