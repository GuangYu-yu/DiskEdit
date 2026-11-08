#!/usr/bin/env bash
# 中间分区操作 + 最小位移扩容 + 数据完整性
source "$(dirname "$0")/lib.sh"
require_bin

img T middle12.img

rm -rf "$T" "$T"$SIDECAR_GLOB
truncate -s 128M "$T"
$B new "$T" --yes || exit 1
$B add "$T" 16M --align mib --name p1 --yes
$B mkfs "$T":1 ext4 --yes
$B add "$T" 16M --align mib --name p2 --yes
$B mkfs "$T":2 ext4 --yes
$B add "$T" 16M --align mib --name p3 --yes
$B mkfs "$T":3 ext4 --yes

LD=$(lo_attach "$T")
mount_at "${LD}p2" "$TEST_MNT" && echo p2-marker-content > "$TEST_MNT"/m2.txt
if [ ! -d /testmnt2 ]; then mkdir -p /testmnt2; fi
mount_at "${LD}p3" /testmnt2
echo p3-marker-content > /testmnt2/m3.txt
# 填充随机数据做完整性校验
dd if=/dev/urandom of="$TEST_MNT/blob.bin" bs=1M count=10 2>/dev/null
umount "$TEST_MNT" /testmnt2
# 整分区字节级哈希：覆盖文件数据之外的全部 FS 元数据；move 是逐字节拷贝，
# 搬移前后的分区设备哈希应当一致
MD3_BEFORE=$(mdsum "${LD}p3")
losetup -d "$LD"

echo "== 中间分区 p2 精确扩 8M（p3 挡路，最小位移）=="
$B resize "$T":2 +8M --allow-move --yes; echo "exit=$?"
$B info "$T"

echo "== 数据完整性 =="
LD=$(lo_attach "$T")
MD3_AFTER=$(mdsum "${LD}p3")
[ "$MD3_BEFORE" = "$MD3_AFTER" ] && echo "p3 DATA OK (moved byte-identical)" || { echo "p3 DATA CORRUPT"; rc=1; }
mount_at "${LD}p2" "$TEST_MNT" && cat "$TEST_MNT"/m2.txt && umount "$TEST_MNT"
losetup -d "$LD"

echo "== 中间分区收缩 -4M =="
$B resize "$T":2 -4M --yes; echo "exit=$?"

echo "== 中间分区 move 到尾部空隙 =="
$B move "$T":2 --start end --yes; echo "exit=$?"
$B info "$T"

echo "== 成功后记录已删，undo 应拒绝（与搬移 undo 拒绝契约一致）=="
$B undo "$T" --yes; echo "undo exit=$? (期望 3)"
LD=$(lo_attach "$T")
# 终态即 move 后布局：p3 纯位移字节一致，p2 内容可读、fsck 干净
MD3_UNDO=$(mdsum "${LD}p3")
[ "$MD3_BEFORE" = "$MD3_UNDO" ] && echo "p3 DATA OK (byte-identical)" || { echo "p3 DATA CORRUPT"; rc=1; }
mount_at "${LD}p2" "$TEST_MNT" && cat "$TEST_MNT"/m2.txt && umount "$TEST_MNT"
e2fsck -fn "${LD}p2" >/dev/null 2>&1; echo "p2 e2fsck exit=$?"
e2fsck -fn "${LD}p3" >/dev/null 2>&1; echo "p3 e2fsck exit=$?"
losetup -d "$LD"
sgdisk -v "$T" | tail -2
exit $rc