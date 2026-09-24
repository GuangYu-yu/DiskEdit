#!/usr/bin/env bash
# 中间分区操作 + 最小位移扩容 + 数据完整性
source "$(dirname "$0")/lib.sh"
require_bin

T=/var/tmp/diskedit_test12.img
track_file "$T"

rm -f "$T" "$T"$SIDECAR_GLOB
truncate -s 128M "$T"
$B new "$T" --yes || exit 1
$B create "$T" --size 16M --name p1 --fs ext4
$B create "$T" --size 16M --name p2 --fs ext4
$B create "$T" --size 16M --name p3 --fs ext4

LD=$(lo_attach "$T")
mount_at "${LD}p2" /testmnt && echo p2-marker-content > /testmnt/m2.txt
if [ ! -d /testmnt2 ]; then mkdir -p /testmnt2; fi
mount_at "${LD}p3" /testmnt2
echo p3-marker-content > /testmnt2/m3.txt
# 填充随机数据做完整性校验
dd if=/dev/urandom of=/testmnt/blob.bin bs=1M count=10 2>/dev/null
umount /testmnt /testmnt2
# 整分区字节级哈希：覆盖文件数据之外的全部 FS 元数据；move 是逐字节拷贝，
# 搬移前后的分区设备哈希应当一致
MD3_BEFORE=$(md5sum "${LD}p3" | cut -d' ' -f1)
losetup -d "$LD"

echo "== 中间分区 p2 精确扩 8M（p3 挡路，最小位移）=="
$B resize "$T":2 +8M --allow-move --yes; echo "exit=$?"
$B info "$T"

echo "== 数据完整性 =="
LD=$(lo_attach "$T")
MD3_AFTER=$(md5sum "${LD}p3" | cut -d' ' -f1)
[ "$MD3_BEFORE" = "$MD3_AFTER" ] && echo "p3 DATA OK (moved byte-identical)" || { echo "p3 DATA CORRUPT"; rc=1; }
mount_at "${LD}p2" /testmnt && cat /testmnt/m2.txt && umount /testmnt
losetup -d "$LD"

echo "== 中间分区收缩 -4M =="
$B resize "$T":2 -4M; echo "exit=$?"

echo "== 中间分区 move 到尾部空隙 =="
$B move "$T":2 --start end; echo "exit=$?"
$B info "$T"

echo "== 成功后 journal 已删，undo 应拒绝（与搬移 undo 拒绝契约一致）=="
$B undo "$T" --yes; echo "undo exit=$? (期望 10)"
LD=$(lo_attach "$T")
# 终态即 move 后布局：p3 纯位移字节一致，p2 内容可读、fsck 干净
MD3_UNDO=$(md5sum "${LD}p3" | cut -d' ' -f1)
[ "$MD3_BEFORE" = "$MD3_UNDO" ] && echo "p3 DATA OK (byte-identical)" || { echo "p3 DATA CORRUPT"; rc=1; }
mount_at "${LD}p2" /testmnt && cat /testmnt/m2.txt && umount /testmnt
e2fsck -fn "${LD}p2" >/dev/null 2>&1; echo "p2 e2fsck exit=$?"
e2fsck -fn "${LD}p3" >/dev/null 2>&1; echo "p3 e2fsck exit=$?"
losetup -d "$LD"
sgdisk -v "$T" | tail -2
exit $rc