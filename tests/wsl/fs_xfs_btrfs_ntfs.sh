#!/usr/bin/env bash
# FS 类型覆盖：xfs / btrfs 扩容，ntfs 搬移（HiddenSectors 修复）
source "$(dirname "$0")/lib.sh"
require_bin

# ntfs-3g 用 -t 挂载，未经 mount_at 登记，退出时兜底卸载
cleanup_hook() { umount /testmnt 2>/dev/null; }

T=/var/tmp/t20.img
track_file "$T"

rm -f "$T" "$T".diskedit.* 2>/dev/null

echo "########## A: xfs 扩容 400M → 1500M ##########"
rm -f "$T" "$T".diskedit.*; truncate -s 2G "$T"
$B new "$T" --yes >/dev/null
$B create "$T" --size 400M --name x --fs xfs; echo "create exit=$?"
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt && dd if=/dev/urandom of=/testmnt/blob bs=1M count=300 2>/dev/null
MX=$(md5sum /testmnt/blob | cut -d' ' -f1)
echo "before: $(df -h --output=size /testmnt | tail -1) md5=$MX"
umount /testmnt 2>/dev/null; lo_detach "$LD"
OUT=$($B resize "$T":1 1500M 2>&1); echo "resize exit=$? out: $(echo "$OUT" | tail -1)"
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt && { echo "after: $(df -h --output=size /testmnt | tail -1)"; G=$(md5sum /testmnt/blob | cut -d' ' -f1); [ "$MX" = "$G" ] && echo "XFS DATA OK" || { echo "XFS DATA CORRUPT"; rc=1; }; umount /testmnt 2>/dev/null; }
xfs_repair -n "${LD}p1" >/dev/null 2>&1; echo "xfs_repair -n exit=$? (0=clean)"
lo_detach "$LD"; sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## B: btrfs 扩容 400M → 1500M ##########"
rm -f "$T" "$T".diskedit.*; truncate -s 2G "$T"
$B new "$T" --yes >/dev/null
$B create "$T" --size 400M --name b --fs btrfs; echo "create exit=$?"
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt && dd if=/dev/urandom of=/testmnt/blob bs=1M count=300 2>/dev/null
MB=$(md5sum /testmnt/blob | cut -d' ' -f1)
echo "before: $(df -h --output=size /testmnt | tail -1)"
umount /testmnt 2>/dev/null; lo_detach "$LD"
OUT=$($B resize "$T":1 1500M 2>&1); echo "resize exit=$? out: $(echo "$OUT" | tail -1)"
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt && { echo "after: $(df -h --output=size /testmnt | tail -1)"; G=$(md5sum /testmnt/blob | cut -d' ' -f1); [ "$MB" = "$G" ] && echo "BTRFS DATA OK" || { echo "BTRFS DATA CORRUPT"; rc=1; }; umount /testmnt 2>/dev/null; }
btrfs check "${LD}p1" >/dev/null 2>&1; echo "btrfs check exit=$? (0=clean)"
lo_detach "$LD"; sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## C: ntfs 搬移（HiddenSectors 修复） ##########"
rm -f "$T" "$T".diskedit.*; truncate -s 1G "$T"
$B new "$T" --yes >/dev/null
$B create "$T" --size 200M --name n --fs ntfs; echo "create exit=$?"
$B create "$T" --size 200M --name filler --fs ext4 >/dev/null
LD=$(lo_attach "$T")
mount -t ntfs-3g "${LD}p1" /testmnt 2>/dev/null || mount_at "${LD}p1" /testmnt
dd if=/dev/urandom of=/testmnt/blob bs=1M count=150 2>/dev/null
MN=$(md5sum /testmnt/blob | cut -d' ' -f1)
OLDS=$($B info "$T" | grep -o '"num":1,"first_lba":[0-9]*' | grep -o '[0-9]*$')
echo "before: hidden=$(od -An -tu4 -j $((OLDS*512+28)) -N4 "$T" | tr -d ' ')"
umount /testmnt 2>/dev/null; lo_detach "$LD"
MOVE_OUT=$($B move "$T":1 --start 900000 --chunk-size 4 2>&1); exp $? 0 "move（NTFS 重定位）"
printf '%s\n' "$MOVE_OUT" | tail -2
NEWS=$($B info "$T" | grep -o '"num":1,"first_lba":[0-9]*' | grep -o '[0-9]*$')
HID=$(od -An -tu4 -j $((NEWS*512+28)) -N4 "$T" | tr -d ' ')
echo "after: first_lba=$NEWS hidden=$HID"
[ "$HID" = "$NEWS" ] && echo "NTFS HIDDENSECTORS OK" || { echo "NTFS HIDDENSECTORS WRONG"; rc=1; }
LD=$(lo_attach "$T")
mount -t ntfs-3g "${LD}p1" /testmnt 2>/dev/null || mount_at "${LD}p1" /testmnt
if mountpoint -q /testmnt; then
  G=$(md5sum /testmnt/blob | cut -d' ' -f1)
  [ "$MN" = "$G" ] && echo "NTFS DATA OK" || { echo "NTFS DATA CORRUPT"; rc=1; }
  umount /testmnt 2>/dev/null
fi
lo_detach "$LD"; sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## D: btrfs 多设备拒绝（num_devices > 1 不可按单成员扩）##########"
# 依据：src/fsops.rs:788-797 refuse_btrfs_multi_device_at 读主 superblock
# num_devices（分区起点 + 0x10000 + 0x88，u64 LE）> 1 即拒绝；grow 侧调用点
# src/fsops.rs:987-996（resize_fs → resize_fs_in），失败经 src/outcome.rs:214 归
# UnsupportedFs → refused（10，未写盘）。
IMGD=/var/tmp/t20d.img
D2=/var/tmp/t20d2.img
track_file "$IMGD"
track_file "$D2"
rm -f "$IMGD" "$IMGD".diskedit.* "$D2" "$D2".diskedit.* /var/tmp/t20d*.img* 2>/dev/null
truncate -s 1G "$IMGD"
$B new "$IMGD" --yes >/dev/null
$B add "$IMGD" --start 2048 --end 1044479 --name btrmd >/dev/null
truncate -s 512M "$D2"
LDM=$(lo_attach "$IMGD")
LDS=$(lo_attach "$D2")
mkfs.btrfs -f -d single -m single "${LDM}p1" "$LDS" >/dev/null 2>&1; echo "mkfs.btrfs(2 dev) exit=$?"
ND=$(od -An -tu8 -j $((2048 * 512 + 0x10000 + 0x88)) -N8 "$IMGD" | tr -d ' ')
echo "num_devices=$ND (期望 2)"
OUT=$($B resizefs "$IMGD":1 2>&1); E=$?
echo "resizefs exit=$E : $(echo "$OUT" | tail -1)"
if [ "$E" = "10" ] && echo "$OUT" | grep -q "spans multiple devices"; then
  echo "D BTRFS-MULTIDEV REFUSED OK"
else
  echo "D WRONG (exit=$E, want 10 + 'spans multiple devices')"; rc=1
fi
lo_detach "$LDM"; lo_detach "$LDS"
sgdisk -v "$IMGD" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc