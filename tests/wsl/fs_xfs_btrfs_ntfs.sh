#!/usr/bin/env bash
# FS 类型覆盖：xfs / btrfs 扩容，ntfs 搬移（HiddenSectors 修复）
source "$(dirname "$0")/lib.sh"
require_bin

# ntfs-3g 用 -t 挂载，未经 mount_at 登记，退出时兜底卸载
cleanup_hook() { umount "$TEST_MNT" 2>/dev/null; }

img T t20.img

rm_img "$T" 2>/dev/null

echo "########## A: xfs 扩容 400M → 1500M ##########"
rm_img "$T"; truncate -s 2G "$T"
$B new "$T" --yes >/dev/null; exp $? 0 "new（A）"
$B add "$T" 400M --align mib --name x --yes >/dev/null; exp $? 0 "add x（400M）"
$B mkfs "$T":1 xfs --yes; exp $? 0 "mkfs xfs"
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$TEST_MNT" && dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=300 2>/dev/null
MX=$(mdsum "$TEST_MNT"/blob)
echo "before: $(df -h --output=size "$TEST_MNT" | tail -1) md5=$MX"
umount "$TEST_MNT" 2>/dev/null; lo_detach "$LD"
OUT=$($B resize --yes "$T":1 1500M 2>&1); E=$?
echo "resize exit=$E : $(echo "$OUT" | tail -1)"
# xfs 没有设备级回读：工具成功但尺寸未证实 → 5，待办给出挂载后的尺寸核对
[ "$E" = "5" ] && echo "A XFS-GROW-UNVERIFIED OK" || { echo "A WRONG EXIT ($E, want 5)"; echo "$OUT"; rc=1; }
echo "$OUT" | grep -q "df -B1" && echo "A XFS SIZE-CHECK HINT OK" || { echo "A NO SIZE-CHECK HINT"; rc=1; }
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$TEST_MNT" && { echo "after: $(df -h --output=size "$TEST_MNT" | tail -1)"; G=$(mdsum "$TEST_MNT"/blob); [ "$MX" = "$G" ] && echo "XFS DATA OK" || { echo "XFS DATA CORRUPT"; rc=1; }; umount "$TEST_MNT" 2>/dev/null; }
fs_meta_check xfs "${LD}p1" "A"; lo_detach "$LD"; sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## B: btrfs 扩容 400M → 1500M ##########"
rm_img "$T"; truncate -s 2G "$T"
$B new "$T" --yes >/dev/null; exp $? 0 "new（B）"
$B add "$T" 400M --align mib --name b --yes >/dev/null; exp $? 0 "add b（400M）"
$B mkfs "$T":1 btrfs --yes; exp $? 0 "mkfs btrfs"
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$TEST_MNT" && dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=300 2>/dev/null
MB=$(mdsum "$TEST_MNT"/blob)
echo "before: $(df -h --output=size "$TEST_MNT" | tail -1)"
umount "$TEST_MNT" 2>/dev/null; lo_detach "$LD"
OUT=$($B resize --yes "$T":1 1500M 2>&1); E=$?
echo "resize exit=$E out: $(echo "$OUT" | tail -1)"
# btrfs 的尺寸可由超级块回读（src/fsops/probe.rs 的 dump-super total_bytes），故走证实路径
exp "$E" 0 "btrfs 扩容 400M → 1500M" || echo "$OUT"
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$TEST_MNT" && { echo "after: $(df -h --output=size "$TEST_MNT" | tail -1)"; G=$(mdsum "$TEST_MNT"/blob); [ "$MB" = "$G" ] && echo "BTRFS DATA OK" || { echo "BTRFS DATA CORRUPT"; rc=1; }; umount "$TEST_MNT" 2>/dev/null; }
fs_meta_check btrfs "${LD}p1" "B"; lo_detach "$LD"; sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## C: ntfs 搬移（HiddenSectors 修复） ##########"
rm_img "$T"; truncate -s 1G "$T"
$B new "$T" --yes >/dev/null; exp $? 0 "new（C）"
$B add "$T" 200M --align mib --name n --yes >/dev/null; exp $? 0 "add n（200M）"
$B mkfs "$T":1 ntfs --yes; exp $? 0 "mkfs ntfs"
$B add "$T" 200M --align mib --name filler --yes >/dev/null; exp $? 0 "add filler"
$B mkfs "$T":2 ext4 --yes >/dev/null; exp $? 0 "mkfs ext4（filler）"
LD=$(lo_attach "$T")
mount -t ntfs-3g "${LD}p1" "$TEST_MNT" 2>/dev/null || mount_at "${LD}p1" "$TEST_MNT"
dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=150 2>/dev/null
mkdir "$TEST_MNT"/d && dd if=/dev/urandom of="$TEST_MNT"/d/sub bs=1M count=8 2>/dev/null
MN=$(mdsum "$TEST_MNT"/blob); MS=$(mdsum "$TEST_MNT"/d/sub)
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
  # 搬移把 inode 带到新偏移，子目录文件是树结构完好的最小正面证据
  S=$(mdsum "$TEST_MNT"/d/sub)
  [ "$MS" = "$S" ] && echo "NTFS SUBDIR DATA OK" || { echo "NTFS SUBDIR DATA CORRUPT"; rc=1; }
  umount "$TEST_MNT" 2>/dev/null
fi
fs_meta_check ntfs "${LD}p1" "C"
lo_detach "$LD"; sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## D: btrfs 多设备拒绝（num_devices > 1 不可按单成员扩）##########"
# 依据：src/fsops/mkfs.rs 的 refuse_btrfs_multi_device_at 读主 superblock
# num_devices（分区起点 + 0x10000 + 0x88，u64 LE）> 1 即拒绝；grow 侧调用点
# src/fsops/grow.rs 的 resize_fs → resize_fs_in，失败经 src/outcome.rs 归
# UnsupportedFs → unsupported（6，未写盘）。
img IMGD t20d.img
img D2 t20d2.img
rm_img "$IMGD"; rm_img "$D2" 2>/dev/null
truncate -s 1G "$IMGD"
$B new "$IMGD" --yes >/dev/null; exp $? 0 "new"
$B add --yes "$IMGD" 533725184 --start 2048 --name btrmd >/dev/null; exp $? 0 "add btrmd"
truncate -s 512M "$D2"
LDM=$(lo_attach "$IMGD")
LDS=$(lo_attach "$D2")
lo_waitpart "${LDM}p1" || { echo "SKIP: ${LDM}p1 未就绪"; exit 0; }
mkfs.btrfs -f -d single -m single "${LDM}p1" "$LDS" >/dev/null 2>&1; exp $? 0 "mkfs.btrfs（2 个设备）"
ND=$(od -An -tu8 -j $((2048 * 512 + 0x10000 + 0x88)) -N8 "$IMGD" | tr -d ' ')
echo "num_devices=$ND (期望 2)"
OUT=$($B resizefs --yes "$IMGD":1 2>&1); E=$?
echo "resizefs exit=$E : $(echo "$OUT" | tail -1)"
if [ "$E" = "6" ] && echo "$OUT" | grep -q "spans multiple devices"; then
  echo "D BTRFS-MULTIDEV-REFUSED OK"
else
  echo "D WRONG (exit=$E, want 6 + 'spans multiple devices')"
  # 失败时全文输出：tail -1 只剩附带诊断行，首行（记录归属、busy/拒绝原因）才是定性证据
  echo "$OUT"
  rc=1
fi
lo_detach "$LDM"; lo_detach "$LDS"
sgdisk_check "$IMGD"
exit $rc