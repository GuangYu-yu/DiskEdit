#!/usr/bin/env bash
# 尺寸单位与拒绝路径：create --size 单位/非法值、--fs 缺值、resizefs --online 单位、resize --size 单位
source "$(dirname "$0")/lib.sh"
require_bin

T=/var/tmp/diskedit_test11.img
track_file "$T"

rm -f "$T" "$T$JOURNAL_SUFFIX"
truncate -s 64M "$T"
$B new "$T" --yes
echo "== create --size 32M (units) =="
$B create "$T" --size 32M --name test1 --fs ext4; exp $? 0 "create --size 32M（单位）"
echo "== create --size +8M (must refuse with message) =="
$B create "$T" --size +8M 2>&1; exp $? 10 "create --size +8M 拒绝"
echo "== create --size abc (must refuse with message) =="
$B create "$T" --size abc 2>&1; exp $? 10 "create --size abc 拒绝"
echo "== create --fs without value (must refuse with message) =="
$B create "$T" --fs 2>&1; exp $? 10 "create --fs 缺值拒绝"
echo "== resizefs online with unit =="
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt
$B resizefs /testmnt 32M --online; exp $? 0 "resizefs online（单位）"
df -h /testmnt | tail -1
umount /testmnt; losetup -d "$LD"
echo "== resize --size with unit via flag =="
$B resize "$T":1 --size 40M; exp $? 0 "resize --size 40M（单位）"
$B info "$T"
LD=$(lo_attach "$T"); e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "e2fsck clean" || { echo "e2fsck ISSUES"; rc=1; }; losetup -d "$LD"
sgdisk -v "$T" | tail -2
exit $rc