#!/usr/bin/env bash
# FS 类型覆盖：xfs / btrfs 扩容，ntfs 搬移（HiddenSectors 修复）
source "$(dirname "$0")/lib.sh"
require_bin

# ntfs-3g 用 -t 挂载，未经 mount_at 登记，退出时兜底卸载
cleanup_hook() { umount "$TEST_MNT" 2>/dev/null; }

img T t20.img

rm -rf "$T" "$T"$SIDECAR_GLOB 2>/dev/null

echo "########## A: xfs 扩容 400M → 1500M ##########"
rm -rf "$T" "$T"$SIDECAR_GLOB; truncate -s 2G "$T"
$B new "$T" --yes >/dev/null
$B add "$T" 400M --align mib --name x --yes >/dev/null; echo "add exit=$?"
$B mkfs "$T":1 xfs --yes; echo "mkfs exit=$?"
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$TEST_MNT" && dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=300 2>/dev/null
MX=$(mdsum "$TEST_MNT"/blob)
echo "before: $(df -h --output=size "$TEST_MNT" | tail -1) md5=$MX"
umount "$TEST_MNT" 2>/dev/null; lo_detach "$LD"
OUT=$($B resize "$T":1 1500M 2>&1); echo "resize exit=$? out: $(echo "$OUT" | tail -1)"
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$TEST_MNT" && { echo "after: $(df -h --output=size "$TEST_MNT" | tail -1)"; G=$(mdsum "$TEST_MNT"/blob); [ "$MX" = "$G" ] && echo "XFS DATA OK" || { echo "XFS DATA CORRUPT"; rc=1; }; umount "$TEST_MNT" 2>/dev/null; }
xfs_repair -n "${LD}p1" >/dev/null 2>&1; echo "xfs_repair -n exit=$? (0=clean)"
lo_detach "$LD"; sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## B: btrfs 扩容 400M → 1500M ##########"
rm -rf "$T" "$T"$SIDECAR_GLOB; truncate -s 2G "$T"
$B new "$T" --yes >/dev/null
$B add "$T" 400M --align mib --name b --yes >/dev/null; echo "add exit=$?"
$B mkfs "$T":1 btrfs --yes; echo "mkfs exit=$?"
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$TEST_MNT" && dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=300 2>/dev/null
MB=$(mdsum "$TEST_MNT"/blob)
echo "before: $(df -h --output=size "$TEST_MNT" | tail -1)"
umount "$TEST_MNT" 2>/dev/null; lo_detach "$LD"
OUT=$($B resize "$T":1 1500M 2>&1); echo "resize exit=$? out: $(echo "$OUT" | tail -1)"
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$TEST_MNT" && { echo "after: $(df -h --output=size "$TEST_MNT" | tail -1)"; G=$(mdsum "$TEST_MNT"/blob); [ "$MB" = "$G" ] && echo "BTRFS DATA OK" || { echo "BTRFS DATA CORRUPT"; rc=1; }; umount "$TEST_MNT" 2>/dev/null; }
btrfs check "${LD}p1" >/dev/null 2>&1; echo "btrfs check exit=$? (0=clean)"
lo_detach "$LD"; sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## C: ntfs 搬移（HiddenSectors 修复） ##########"
rm -rf "$T" "$T"$SIDECAR_GLOB; truncate -s 1G "$T"
$B new "$T" --yes >/dev/null
$B add "$T" 200M --align mib --name n --yes >/dev/null; echo "add exit=$?"
$B mkfs "$T":1 ntfs --yes; echo "mkfs exit=$?"
$B add "$T" 200M --align mib --name filler --yes >/dev/null
$B mkfs "$T":2 ext4 --yes >/dev/null
LD=$(lo_attach "$T")
mount -t ntfs-3g "${LD}p1" "$TEST_MNT" 2>/dev/null || mount_at "${LD}p1" "$TEST_MNT"
dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=150 2>/dev/null
MN=$(mdsum "$TEST_MNT"/blob)
OLDS=$($B info "$T" | grep -o '"num":1,"first_lba":[0-9]*' | grep -o '[0-9]*$')
echo "before: hidden=$(od -An -tu4 -j $((OLDS*512+28)) -N4 "$T" | tr -d ' ')"
umount "$TEST_MNT" 2>/dev/null; lo_detach "$LD"
MOVE_OUT=$($B move "$T":1 --start 900000 --chunk 4 --yes 2>&1); exp $? 0 "move（NTFS 重定位）"
printf '%s\n' "$MOVE_OUT" | tail -2
NEWS=$($B info "$T" | grep -o '"num":1,"first_lba":[0-9]*' | grep -o '[0-9]*$')
HID=$(od -An -tu4 -j $((NEWS*512+28)) -N4 "$T" | tr -d ' ')
echo "after: first_lba=$NEWS hidden=$HID"
[ "$HID" = "$NEWS" ] && echo "NTFS HIDDENSECTORS OK" || { echo "NTFS HIDDENSECTORS WRONG"; rc=1; }
LD=$(lo_attach "$T")
mount -t ntfs-3g "${LD}p1" "$TEST_MNT" 2>/dev/null || mount_at "${LD}p1" "$TEST_MNT"
if mountpoint -q "$TEST_MNT"; then
  G=$(mdsum "$TEST_MNT"/blob)
  [ "$MN" = "$G" ] && echo "NTFS DATA OK" || { echo "NTFS DATA CORRUPT"; rc=1; }
  umount "$TEST_MNT" 2>/dev/null
fi
lo_detach "$LD"; sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## D: btrfs 多设备拒绝（num_devices > 1 不可按单成员扩）##########"
# 依据：src/fsops/mkfs.rs:213 refuse_btrfs_multi_device_at 读主 superblock
# num_devices（分区起点 + 0x10000 + 0x88，u64 LE）> 1 即拒绝；grow 侧调用点
# src/fsops/grow.rs:16,146（resize_fs → resize_fs_in），失败经 src/outcome.rs:339 归
# UnsupportedFs → unsupported（6，未写盘）。
img IMGD t20d.img
img D2 t20d2.img
rm -rf "$IMGD" "$IMGD"$SIDECAR_GLOB "$D2" "$D2"$SIDECAR_GLOB 2>/dev/null
truncate -s 1G "$IMGD"
$B new "$IMGD" --yes >/dev/null
$B add "$IMGD" 533725184 --start 2048 --name btrmd >/dev/null
truncate -s 512M "$D2"
LDM=$(lo_attach "$IMGD")
LDS=$(lo_attach "$D2")
mkfs.btrfs -f -d single -m single "${LDM}p1" "$LDS" >/dev/null 2>&1; echo "mkfs.btrfs(2 dev) exit=$?"
ND=$(od -An -tu8 -j $((2048 * 512 + 0x10000 + 0x88)) -N8 "$IMGD" | tr -d ' ')
echo "num_devices=$ND (期望 2)"
OUT=$($B resizefs "$IMGD":1 2>&1); E=$?
echo "resizefs exit=$E : $(echo "$OUT" | tail -1)"
if [ "$E" = "6" ] && echo "$OUT" | grep -q "spans multiple devices"; then
  echo "D BTRFS-MULTIDEV-REFUSED OK"
else
  echo "D WRONG (exit=$E, want 6 + 'spans multiple devices')"; rc=1
fi
lo_detach "$LDM"; lo_detach "$LDS"
sgdisk -v "$IMGD" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc