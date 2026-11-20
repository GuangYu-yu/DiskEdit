#!/usr/bin/env bash
# MBR 全链路 + delete（GPT/MBR）
# 覆盖：new --table mbr / add --type-guid 0xNN / resize 扩缩 / flag boot|hidden /
#       扩展容器拒绝 / esp flag 拒绝 / delete（--yes 确认、数据区不擦除、GPT 侧）
source "$(dirname "$0")/lib.sh"
require_bin



img T t29.img

rm_img "$T" 2>/dev/null

echo "===== A: MBR 建表 / add --type-guid / flag ====="
rm_img "$T"
truncate -s 256M "$T"
$B new "$T" --table mbr --yes; exp "$?" 0 "new mbr"
$B add --yes "$T" 50151424 --start 2048 >/dev/null; exp "$?" 0 "add p1"
# p2 起点留出 p1 扩到 48M（末扇区 100351）所需的空间：MBR 无搬移能力，紧邻会被拒绝
$B add --yes "$T" 51200000 --start 120000 --type-guid 0x07 >/dev/null; exp "$?" 0 "add p2(0x07)"
$B info "$T" | grep -o '"label":"[a-z]*"'
$B set --yes "$T":1 flag boot on; exp "$?" 0 "flag boot"
sfdisk -d "$T" 2>/dev/null | grep -o 'bootable' | head -1
OUT=$($B set --yes "$T":1 flag esp on 2>&1); E=$?
echo "$OUT" | head -1
[ "$E" = "3" ] && echo "flag esp on MBR refused OK (exit=$E)" || { echo "flag esp on MBR wrong exit ($E, want 3)"; rc=1; }
# hidden 仅对 FAT/NTFS 类型有隐藏变体映射
$B set --yes "$T":1 flag hidden on >/dev/null 2>&1; E=$?
[ "$E" = "3" ] && echo "hidden on 0x83 refused OK (exit=$E)" || { echo "hidden on 0x83 wrong exit ($E, want 3)"; rc=1; }
OUT=$($B set --yes "$T":2 flag hidden on 2>&1); E=$?
echo "$OUT" | head -1
exp "$E" 0 "hidden on 0x07"
$B set --yes "$T":2 flag hidden off >/dev/null 2>&1; exp "$?" 0 "hidden off"

echo
echo "===== B: MBR resize 扩 / 缩 ====="
LD=$(lo_attach "$T")
mkfs.ext4 -q "${LD}p1"
mount_at "${LD}p1" "$TEST_MNT" && { echo marker-mbr > "$TEST_MNT"/m.txt; dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=20 2>/dev/null; umount "$TEST_MNT" 2>/dev/null; }
lo_detach "$LD"
$B resize --yes "$T":1 48M >/dev/null 2>&1; exp "$?" 0 "grow"
$B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*'
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$TEST_MNT" && { cat "$TEST_MNT"/m.txt; e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "grow e2fsck clean" || { echo "grow e2fsck ISSUES"; rc=1; }; umount "$TEST_MNT" 2>/dev/null; }
lo_detach "$LD"
# MBR 主分区与 GPT 走同一条收缩路径（先离线缩 FS，再收条目）；--end 形态仍只对 GPT
# （src/gpt_policy.rs 的 require_gpt_geometry）
OUT=$($B resize "$T":1 30M --yes 2>&1); E=$?
echo "$OUT" | grep -E 'fs shrunk|refused|error' | sed 's/^/    /'
exp "$E" 0 "shrink"
$B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*'
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$TEST_MNT" && { cat "$TEST_MNT"/m.txt; e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "shrink e2fsck clean" || { echo "shrink e2fsck ISSUES"; rc=1; }; umount "$TEST_MNT" 2>/dev/null; }
lo_detach "$LD"

echo
echo "===== C: 扩展容器不可 resize ====="
# 产品的 add 拒绝构造扩展容器（logical partitions are out of scope），夹具改用普通条目 +
# 外部改类型字节：MBR 第 3 条目的类型字节在 446 + 2*16 + 4 = 482（p3 起点 240000 避开 p2）
$B add --yes "$T" 98926592 --start 240000 >/dev/null; exp "$?" 0 "add p3"
printf '\x05' | dd of="$T" bs=1 seek=482 conv=notrunc 2>/dev/null
$B info "$T" | grep -o '"num":3[^}]*}' | head -1
OUT=$($B resize --yes "$T":3 100M 2>&1); E=$?
echo "$OUT" | head -1
[ "$E" = "3" ] && echo "resize container refused OK (exit=$E)" || { echo "resize container wrong exit ($E, want 3)"; rc=1; }

echo
echo "===== D: delete（数据区不擦除）====="
LD=$(lo_attach "$T")
dd if=/dev/urandom of="${LD}p2" bs=1M count=8 2>/dev/null
MD2=$(head -c 8388608 "${LD}p2" | md5sum | cut -d' ' -f1)
# 分区删除后节点消失，从整盘按 p2 起点 LBA 读同区域比对
# （MBR info 的字段序是 type 在 first_lba 前，需先截取该分区对象再取字段）
P2START=$($B info "$T" | grep -o '"num":2,[^}]*}' | grep -o '"first_lba":[0-9]*' | grep -o '[0-9]*$')
lo_detach "$LD"
OUT=$($B delete "$T":2 2>&1); E=$?
echo "$OUT" | head -1
# 缺 --yes 的确认门在命令面层拒绝（usage 2），不进入占用/几何判定
[ "$E" = "2" ] && echo "delete w/o --yes usage-refused OK (exit=$E)" || { echo "delete w/o --yes wrong exit ($E, want 2)"; rc=1; }
$B delete "$T":2 --yes >/dev/null 2>&1; exp "$?" 0 "delete p2"
if $B info "$T" | grep -q '"num":2'; then echo "p2 STILL PRESENT (BAD)"; rc=1; else echo "p2 GONE OK"; fi
LD=$(lo_attach "$T")
[ "$MD2" = "$(dd if="$LD" bs=512 skip=$P2START count=16384 2>/dev/null | md5sum | cut -d' ' -f1)" ] && echo "p2 DATA-AREA NOT WIPED OK" || { echo "p2 DATA-AREA WIPED (BAD)"; rc=1; }
lo_detach "$LD"

echo
echo "===== E: GPT 侧 delete ====="
rm_img "$T"
truncate -s 64M "$T"
$B new "$T" --yes >/dev/null
$B add --yes "$T" 50151424 --start 2048 --name g1 >/dev/null
$B delete "$T":1 --yes >/dev/null 2>&1; exp "$?" 0 "gpt delete"
if $B info "$T" | grep -q '"num":1'; then echo "g1 STILL PRESENT (BAD)"; rc=1; else echo "g1 GONE OK"; fi
sgdisk_check "$T"
exit $rc
