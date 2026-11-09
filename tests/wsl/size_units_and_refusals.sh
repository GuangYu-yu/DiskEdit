#!/usr/bin/env bash
# 尺寸单位与拒绝路径：add 位置槽单位/非法值、mkfs 缺 FS、resizefs 挂载点形式单位、resize 位置参数单位
source "$(dirname "$0")/lib.sh"
require_bin

img T units11.img

rm -rf "$T" "$T"$SIDECAR_GLOB
truncate -s 64M "$T"
$B new "$T" --yes
echo "== add 32M + mkfs ext4 (units) =="
$B add "$T" 32M --align mib --name test1 --yes; exp $? 0 "add 32M（单位）"
$B mkfs "$T":1 ext4 --yes; exp $? 0 "mkfs ext4"
echo "== add +8M (must refuse with message) =="
$B add "$T" +8M --align mib --yes 2>&1; exp $? 2 "add +8M 拒绝"
echo "== add abc (must refuse with message) =="
$B add "$T" abc --align mib --yes 2>&1; exp $? 2 "add abc 拒绝"
echo "== mkfs without the FS argument (must refuse with message) =="
$B mkfs "$T":1 --yes 2>&1; exp $? 2 "mkfs 缺 FS 拒绝"
echo "== resizefs online with unit =="
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$TEST_MNT"
$B resizefs --yes "$TEST_MNT" 32M; exp $? 0 "resizefs 挂载点形式（单位）"
df -h "$TEST_MNT" | tail -1
umount "$TEST_MNT"; losetup -d "$LD"
echo "== resize SIZE with unit (positional) =="
$B resize --yes "$T":1 40M; exp $? 0 "resize 40M（单位）"
$B info "$T"
LD=$(lo_attach "$T"); e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "e2fsck clean" || { echo "e2fsck ISSUES"; rc=1; }; losetup -d "$LD"
sgdisk -v "$T" | tail -2
exit $rc