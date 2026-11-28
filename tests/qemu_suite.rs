//! 真实内核行为套件：在 QEMU 来宾里用 root 驱动本 crate 的测试二进制，对真实 loop
//! 块设备断言盘面与内核结果。
//!
//! 宿主没有 CAP_SYS_ADMIN、内核也不允许在容器里做分区扫描/挂载/device-mapper，
//! 这些路径无法在进程内直接测。这里把同一份二进制放进带完整工具的来宾里跑：
//! 分区表写入、`BLKPG` 同步、挂载中的在线扩容、swap、LVM、移动与 mkfs 都在真内核上
//! 验证，而不是用假端口模拟。
//!
//! 运行方式（默认跳过，避免无 QEMU/无网络的环境里误报）：
//!
//! ```text
//! DISKEDIT_QEMU=1 cargo test --test qemu_suite -- --nocapture
//! ```
//!
//! 可用环境变量：
//! - `DISKEDIT_QEMU_ROOTFS`：复用已备好的 rootfs 目录（含 `/root`、`/lib/modules`）。
//! - `DISKEDIT_QEMU_KERNEL`：来宾内核，默认 `/boot/vmlinuz-$(uname -r)`。
//! - `DISKEDIT_QEMU_MIRROR`：debootstrap 镜像，默认 `http://mirrors.volces.com/ubuntu`。
//! - `DISKEDIT_QEMU_CACHE`：缓存目录，默认 `target/qemu`。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 来宾 PID 1：挂伪文件系统、清空上次运行的状态、跑场景、断电。
const INIT: &str = r##"#!/bin/sh
exec >/dev/console 2>&1
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
export HOME=/root
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /dev/pts /dev/shm
mount -t devpts devpts /dev/pts 2>/dev/null
mount -t tmpfs tmpfs /dev/shm 2>/dev/null
mount -t tmpfs -o size=1500M tmpfs /tmp 2>/dev/null
# 每次启动从零状态开始：记录目录留在镜像里会跨运行污染下一次的身份与恢复断言。
rm -rf /root/.local/state/diskedit
echo "=== VM_BOOT_OK kernel=$(uname -r) ==="
/bin/sh /root/scenarios.sh
echo "=== VM_RESULT_DONE ==="
sync
echo 1 >/proc/sys/kernel/sysrq 2>/dev/null
echo o >/proc/sysrq-trigger 2>/dev/null
sleep 5
"##;

/// 来宾里执行的场景脚本。每个场景都以标准工具核对盘面结果。
const SCENARIOS: &str = r##"#!/bin/sh
# Real-kernel behaviour tests for diskedit, run as root inside a QEMU VM.
DE=/root/diskedit
PASS=0; FAIL=0; FAILED=""
ok()  { PASS=$((PASS+1)); echo "PASS $1"; }
no()  { FAIL=$((FAIL+1)); FAILED="$FAILED $1"; echo "FAIL $1 :: $2"; [ -s /tmp/o ] && sed 's/^/   | /' /tmp/o; }
assert_rc() { [ "$2" = "$3" ] && ok "$1" || no "$1" "rc expected $2 got $3"; }
assert_gt() { [ "$2" -gt "$3" ] 2>/dev/null && ok "$1" || no "$1" "$2 not > $3"; }
assert_ge() { [ "$2" -ge "$3" ] 2>/dev/null && ok "$1" || no "$1" "$2 not >= $3"; }
assert_le() { [ "$2" -le "$3" ] 2>/dev/null && ok "$1" || no "$1" "$2 not <= $3"; }
assert_eq() { [ "$2" = "$3" ] && ok "$1" || no "$1" "expected [$2] got [$3]"; }

bytes() { blockdev --getsize64 "$1" 2>/dev/null || echo 0; }
blkcount() { dumpe2fs -h "$1" 2>/dev/null | awk -F: '/^Block count/{gsub(/ /,"",$2);print $2}'; }
# f2fs superblock 里 block_count 位于结构内偏移 36、绝对偏移 1024+36=1060（小端 u64）。
f2fs_blocks() { od -An -tu8 -j1060 -N8 "$1" 2>/dev/null | tr -d ' '; }
detach() { [ -n "$1" ] && { umount "${1}p1" 2>/dev/null; vgchange -an vgtest 2>/dev/null; losetup -d "$1" 2>/dev/null; }; }
newdisk() { dd if=/dev/zero of="$1" bs=1M count="$2" status=none; L=$(losetup -fP --show "$1"); sleep 1; }

# S1: GPT + ext4，离线扩到盘尾。
s1_offline_ext4_grow() {
  newdisk /tmp/s1.img 300
  printf 'label: gpt\n, 120M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  mkfs.ext4 -q -F "${L}p1"
  before=$(bytes "${L}p1"); b_before=$(blkcount "${L}p1")
  "$DE" resize "$L:1" --grow-to-end --yes >/tmp/o 2>&1; rc=$?
  after=$(bytes "${L}p1")
  assert_rc s1_grow_rc 0 "$rc"
  assert_gt s1_part_grew "$after" "$before"
  e2fsck -fn "${L}p1" >/dev/null 2>&1; assert_rc s1_fs_clean 0 "$?"
  assert_gt s1_fs_grew "$(blkcount "${L}p1")" "$b_before"
  detach "$L"; rm -f /tmp/s1.img
}

# S2: GPT + ext4，离线收缩（--end 到约 50 MiB）。
s2_offline_ext4_shrink() {
  newdisk /tmp/s2.img 300
  printf 'label: gpt\n, 200M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  mkfs.ext4 -q -F "${L}p1"
  before=$(bytes "${L}p1")
  "$DE" resize "$L:1" --end 104447 --yes >/tmp/o 2>&1; rc=$?
  after=$(bytes "${L}p1")
  assert_rc s2_shrink_rc 0 "$rc"
  assert_gt s2_part_shrank "$before" "$after"
  e2fsck -fn "${L}p1" >/dev/null 2>&1; assert_rc s2_fs_clean 0 "$?"
  detach "$L"; rm -f /tmp/s2.img
}

# S3: MBR + ext4，离线扩容。
s3_mbr_ext4_grow() {
  newdisk /tmp/s3.img 300
  printf 'label: dos\n, 120M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  mkfs.ext4 -q -F "${L}p1"
  before=$(bytes "${L}p1")
  "$DE" resize "$L:1" --grow-to-end --yes >/tmp/o 2>&1; rc=$?
  assert_rc s3_grow_rc 0 "$rc"
  assert_gt s3_part_grew "$(bytes "${L}p1")" "$before"
  e2fsck -fn "${L}p1" >/dev/null 2>&1; assert_rc s3_fs_clean 0 "$?"
  detach "$L"; rm -f /tmp/s3.img
}

# S4: btrfs 离线扩容（收缩分支见 tests/qemu_suite.rs 的说明）。
s4_btrfs_grow() {
  modprobe btrfs 2>/dev/null
  newdisk /tmp/s4.img 320
  printf 'label: gpt\n, 200M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  mkfs.btrfs -q -f "${L}p1" >/dev/null 2>&1 || { no s4_mkfs "mkfs.btrfs failed"; detach "$L"; rm -f /tmp/s4.img; return; }
  "$DE" resize "$L:1" --grow-to-end --yes >/tmp/o 2>&1; rc=$?
  assert_rc s4_grow_rc 0 "$rc"
  dev=$(btrfs inspect-internal dump-super "${L}p1" 2>/dev/null | awk '$1=="total_bytes"{print $2}')
  part=$(bytes "${L}p1")
  assert_ge s4_btrfs_within_part "$part" "$dev"
  assert_gt s4_btrfs_grew "$dev" "209715200"
  detach "$L"; rm -f /tmp/s4.img
}

# S5: 挂载中的 ext4 在线扩容。
s5_online_ext4_grow() {
  newdisk /tmp/s5.img 300
  printf 'label: gpt\n, 120M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  mkfs.ext4 -q -F "${L}p1"
  mkdir -p /mnt/s5; mount "${L}p1" /mnt/s5
  before=$(bytes "${L}p1"); fsbefore=$(df -B1 --output=size /mnt/s5 | tail -1)
  "$DE" resize "$L:1" --grow-to-end --yes >/tmp/o 2>&1; rc=$?
  assert_rc s5_online_rc 0 "$rc"
  assert_gt s5_part_grew "$(bytes "${L}p1")" "$before"
  fsafter=$(df -B1 --output=size /mnt/s5 | tail -1)
  assert_gt s5_mounted_fs_grew "$fsafter" "$fsbefore"
  umount /mnt/s5; e2fsck -fn "${L}p1" >/dev/null 2>&1; assert_rc s5_fs_clean 0 "$?"
  detach "$L"; rm -f /tmp/s5.img
}

# S6: swap 扩容并保留卷标。
s6_swap_grow() {
  newdisk /tmp/s6.img 300
  printf 'label: gpt\n, 120M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  mkswap -L SWAPX "${L}p1" >/dev/null 2>&1
  before=$(bytes "${L}p1")
  "$DE" resize "$L:1" --grow-to-end --yes >/tmp/o 2>&1; rc=$?
  assert_rc s6_swap_rc 0 "$rc"
  assert_gt s6_part_grew "$(bytes "${L}p1")" "$before"
  swapon "${L}p1" 2>/tmp/o; assert_rc s6_swapon 0 "$?"
  swsize=$(awk 'NR>1{print $3}' /proc/swaps | head -1)
  assert_ge s6_swap_active "$swsize" "$((before/1024 - 64))"
  swapoff "${L}p1" 2>/dev/null
  lbl=$(swaplabel "${L}p1" 2>/dev/null | awk '/LABEL/{print $2}')
  assert_eq s6_label_preserved "$lbl" "SWAPX"
  detach "$L"; rm -f /tmp/s6.img
}

# S7: LVM 在线 PV 扩容（dm 占用的分区）。
s7_lvm_online_pv_grow() {
  newdisk /tmp/s7.img 320
  printf 'label: gpt\n, 160M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  pvcreate -y -ff "${L}p1" >/dev/null 2>&1 || { no s7_pvcreate "pvcreate failed"; detach "$L"; rm -f /tmp/s7.img; return; }
  vgcreate vgtest "${L}p1" >/dev/null 2>&1 || { no s7_vgcreate "vgcreate failed"; detach "$L"; rm -f /tmp/s7.img; return; }
  # 没有 udev 的来宾里 lvcreate 默认要"擦除"新 LV 却拿不到设备节点；-Z n 跳过置零，
  # vgmknodes 补建 /dev/vgtest 与 /dev/mapper 节点。
  lvcreate -y -l 100%FREE -n lvtest vgtest --wipesignatures n -Z n >/dev/null 2>&1 || { no s7_lvcreate "lvcreate failed"; detach "$L"; rm -f /tmp/s7.img; return; }
  vgmknodes >/dev/null 2>&1
  mkfs.ext4 -q -F /dev/vgtest/lvtest >/dev/null 2>&1
  before=$(bytes "${L}p1")
  pv_before=$(pvs --noheadings --units b --nosuffix -o pv_size "${L}p1" | tr -d ' ')
  "$DE" resize "$L:1" --grow-to-end --yes >/tmp/o 2>&1; rc=$?
  assert_rc s7_online_pv_rc 0 "$rc"
  assert_gt s7_part_grew "$(bytes "${L}p1")" "$before"
  pv_after=$(pvs --noheadings --units b --nosuffix -o pv_size "${L}p1" | tr -d ' ')
  assert_gt s7_pv_grew "$pv_after" "$pv_before"
  detach "$L"; vgremove -ff vgtest 2>/dev/null; pvremove -y "${L}p1" 2>/dev/null
  rm -f /tmp/s7.img
}

# S8: 移动分区并核对数据完整性。
s8_move_preserves_data() {
  newdisk /tmp/s8.img 300
  printf 'label: gpt\n, 60M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  mkfs.ext4 -q -F "${L}p1"
  mkdir -p /mnt/s8; mount "${L}p1" /mnt/s8
  head -c 2000000 /dev/urandom > /mnt/s8/payload
  sum_before=$(md5sum /mnt/s8/payload | awk '{print $1}')
  umount /mnt/s8
  "$DE" move "$L:1" --start 204800 --chunk 1 --yes >/tmp/o 2>&1; rc=$?
  assert_rc s8_move_rc 0 "$rc"
  sleep 1
  mount "${L}p1" /mnt/s8 2>/tmp/o; mrc=$?
  assert_rc s8_mount_after_move 0 "$mrc"
  if [ "$mrc" = 0 ]; then
    sum_after=$(md5sum /mnt/s8/payload | awk '{print $1}')
    assert_eq s8_data_intact "$sum_after" "$sum_before"
    umount /mnt/s8
  fi
  detach "$L"; rm -f /tmp/s8.img
}

# S9: 会自我识别的设备上给 --alias 必须被拒绝且不改盘面。
s9_alias_refused() {
  newdisk /tmp/s9.img 200
  printf 'label: gpt\n, 100M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  before=$(sfdisk -d "$L" 2>/dev/null)
  "$DE" resize "$L:1" --grow-to-end --alias nope --yes >/tmp/o 2>&1; rc=$?
  assert_rc s9_alias_rc 2 "$rc"
  grep -qi alias /tmp/o && ok s9_alias_message || no s9_alias_message "$(cat /tmp/o)"
  after=$(sfdisk -d "$L" 2>/dev/null)
  assert_eq s9_refusal_changed_nothing "$before" "$after"
  detach "$L"; rm -f /tmp/s9.img
}

# S10: 真内核上的建表 / 加分区 / 删分区。
s10_table_edits() {
  newdisk /tmp/s10.img 200
  "$DE" new "$L" --table gpt --yes >/tmp/o 2>&1; rc=$?
  assert_rc s10_new_rc 0 "$rc"
  "$DE" layout "$L" --json >/tmp/o 2>&1
  grep -q '"label":"gpt"' /tmp/o && ok s10_layout_gpt || no s10_layout_gpt "$(cat /tmp/o)"
  "$DE" add "$L" 64M --yes >/tmp/o 2>&1; rc=$?
  assert_rc s10_add_rc 0 "$rc"
  sleep 1
  [ -b "${L}p1" ] && ok s10_kernel_partition || no s10_kernel_partition "no ${L}p1"
  "$DE" delete "$L:1" --yes >/tmp/o 2>&1; rc=$?
  assert_rc s10_delete_rc 0 "$rc"
  sleep 1
  [ -b "${L}p1" ] && no s10_partition_gone "${L}p1 still exists" || ok s10_partition_gone
  detach "$L"; rm -f /tmp/s10.img
}

# S11: 真分区上的 mkfs + check。
s11_mkfs_and_check() {
  newdisk /tmp/s11.img 200
  printf 'label: gpt\n, 100M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  "$DE" mkfs "$L:1" ext4 --yes >/tmp/o 2>&1; rc=$?
  assert_rc s11_mkfs_rc 0 "$rc"
  blkid -o value -s TYPE "${L}p1" >/tmp/o 2>&1
  assert_eq s11_mkfs_type "ext4" "$(cat /tmp/o)"
  "$DE" check "$L:1" >/tmp/o 2>&1; rc=$?
  assert_rc s11_check_rc 0 "$rc"
  detach "$L"; rm -f /tmp/s11.img
}

# S12: 挂载中的 XFS 在线扩容（回读几何核对）。
s12_online_xfs_grow() {
  newdisk /tmp/s12.img 400
  printf 'label: gpt\n, 320M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  mkfs.xfs -q -f "${L}p1" >/dev/null 2>&1 || { no s12_mkfs "mkfs.xfs failed"; detach "$L"; rm -f /tmp/s12.img; return; }
  mkdir -p /mnt/s12; mount "${L}p1" /mnt/s12
  before=$(bytes "${L}p1"); fsbefore=$(df -B1 --output=size /mnt/s12 | tail -1)
  "$DE" resize "$L:1" --grow-to-end --yes >/tmp/o 2>&1; rc=$?
  assert_rc s12_online_rc 0 "$rc"
  assert_gt s12_part_grew "$(bytes "${L}p1")" "$before"
  fsafter=$(df -B1 --output=size /mnt/s12 | tail -1)
  assert_gt s12_mounted_fs_grew "$fsafter" "$fsbefore"
  "$DE" records show "$L" >/tmp/o 2>&1
  grep -q '^record: absent' /tmp/o && ok s12_no_pending_record || no s12_no_pending_record "$(cat /tmp/o)"
  umount /mnt/s12; xfs_repair -n "${L}p1" >/dev/null 2>&1; assert_rc s12_fs_clean 0 "$?"
  detach "$L"; rm -f /tmp/s12.img
}

# S13: vfat mkfs + check。
s13_vfat_mkfs() {
  newdisk /tmp/s13.img 120
  printf 'label: dos\n, 64M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  "$DE" mkfs "$L:1" vfat --yes >/tmp/o 2>&1; rc=$?
  assert_rc s13_mkfs_rc 0 "$rc"
  blkid -o value -s TYPE "${L}p1" >/tmp/o 2>&1
  assert_eq s13_mkfs_type "vfat" "$(cat /tmp/o)"
  "$DE" check "$L:1" >/tmp/o 2>&1; rc=$?
  assert_rc s13_check_rc 0 "$rc"
  detach "$L"; rm -f /tmp/s13.img
}

# S14: 多设备 btrfs 的成员分区不能单独收缩——拒绝且不改盘面。
s14_multidevice_btrfs_refused() {
  modprobe btrfs 2>/dev/null
  for t in a b; do
    dd if=/dev/zero of=/tmp/s14$t.img bs=1M count=240 status=none
    eval "L$t=\$(losetup -fP --show /tmp/s14$t.img)"; sleep 1
    eval "D=\$L$t"; printf 'label: gpt\n, 160M, L\n' | sfdisk "$D" >/dev/null 2>&1; sleep 1
  done
  mkfs.btrfs -q -f -d raid1 -m raid1 "${La}p1" "${Lb}p1" >/tmp/o 2>&1 || {
    no s14_mkfs "multi-device mkfs.btrfs failed"
    losetup -d "$La" 2>/dev/null; losetup -d "$Lb" 2>/dev/null
    rm -f /tmp/s14a.img /tmp/s14b.img; return
  }
  before=$(sfdisk -d "$La" 2>/dev/null)
  "$DE" resize "$La:1" --end 309247 --yes >/tmp/o 2>&1; rc=$?
  assert_rc s14_refused 6 "$rc"
  assert_eq s14_no_write "$before" "$(sfdisk -d "$La" 2>/dev/null)"
  # 成员仍然可挂载：拒绝发生在任何写入之前。
  mkdir -p /mnt/s14; mount "${La}p1" /mnt/s14 2>/tmp/o
  assert_rc s14_fs_intact 0 "$?"
  umount /mnt/s14 2>/dev/null
  losetup -d "$La" 2>/dev/null; losetup -d "$Lb" 2>/dev/null
  rm -f /tmp/s14a.img /tmp/s14b.img
}

# NTFS：mkfs + check 走真实 ntfs-3g。
s15_ntfs_mkfs_check() {
  newdisk /tmp/s15.img 200
  printf 'label: gpt\n, 128M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  "$DE" mkfs "$L:1" ntfs --yes >/tmp/o 2>&1; rc=$?
  assert_rc s15_mkfs_rc 0 "$rc"
  blkid -o value -s TYPE "${L}p1" >/tmp/o 2>&1
  assert_eq s15_mkfs_type "ntfs" "$(cat /tmp/o)"
  "$DE" check "$L:1" >/tmp/o 2>&1; rc=$?
  assert_rc s15_check_rc 0 "$rc"
  detach "$L"; rm -f /tmp/s15.img
}

# exFAT：mkfs + check 走真实 exfatprogs。
s16_exfat_mkfs_check() {
  newdisk /tmp/s16.img 200
  printf 'label: gpt\n, 64M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  "$DE" mkfs "$L:1" exfat --yes >/tmp/o 2>&1; rc=$?
  assert_rc s16_mkfs_rc 0 "$rc"
  blkid -o value -s TYPE "${L}p1" >/tmp/o 2>&1
  assert_eq s16_mkfs_type "exfat" "$(cat /tmp/o)"
  "$DE" check "$L:1" >/tmp/o 2>&1; rc=$?
  assert_rc s16_check_rc 0 "$rc"
  detach "$L"; rm -f /tmp/s16.img
}

# f2fs：mkfs + check 走真实 f2fs-tools。
s17_f2fs_mkfs_check() {
  newdisk /tmp/s17.img 200
  printf 'label: gpt\n, 64M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  "$DE" mkfs "$L:1" f2fs --yes >/tmp/o 2>&1; rc=$?
  assert_rc s17_mkfs_rc 0 "$rc"
  blkid -o value -s TYPE "${L}p1" >/tmp/o 2>&1
  assert_eq s17_mkfs_type "f2fs" "$(cat /tmp/o)"
  "$DE" check "$L:1" >/tmp/o 2>&1; rc=$?
  assert_rc s17_check_rc 6 "$rc"
  grep -q "unsupported" /tmp/o && ok s17_check_message || no s17_check_message "check 未报 unsupported"
  detach "$L"; rm -f /tmp/s17.img
}

# NTFS 扩容：真实 ntfsresize 把卷撑到分区末尾。
s18_ntfs_grow() {
  newdisk /tmp/s18.img 300
  printf 'label: gpt\n, 96M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  mkntfs -q -F "${L}p1" >/dev/null 2>&1 || { no s18_mkfs "mkntfs failed"; detach "$L"; rm -f /tmp/s18.img; return; }
  size_of() { ntfsresize --info --force "$1" 2>&1 | grep 'Current volume size' | grep -o '[0-9][0-9]* bytes' | head -1 | cut -d' ' -f1; }
  before=$(size_of "${L}p1")
  "$DE" resize "$L:1" --grow-to-end --yes >/tmp/o 2>&1; rc=$?
  assert_rc s18_grow_rc 0 "$rc"
  after=$(size_of "${L}p1")
  assert_gt s18_volume_grew "$after" "$before"
  detach "$L"; rm -f /tmp/s18.img
}

# f2fs 离线扩容：真实 fsck.f2fs→resize.f2fs，回读 superblock block_count 核对生长。
s19_f2fs_grow() {
  newdisk /tmp/s19.img 300
  printf 'label: gpt\n, 96M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  "$DE" mkfs "$L:1" f2fs --yes >/tmp/o 2>&1; rc=$?
  assert_rc s19_mkfs_rc 0 "$rc"
  before=$(f2fs_blocks "${L}p1")
  "$DE" resize "$L:1" --grow-to-end --yes >/tmp/o 2>&1; rc=$?
  assert_rc s19_grow_rc 0 "$rc"
  after=$(f2fs_blocks "${L}p1")
  assert_gt s19_fs_grew "$after" "$before"
  detach "$L"; rm -f /tmp/s19.img
}

# vfat 扩容不提供：fatresize 1.1.0 在带偏移的 loop 分区视图上断言崩溃（宿主同版本
# 亦 buffer overflow），决策层在写任何字节前按不支持拒绝。
s20_vfat_grow_refused() {
  newdisk /tmp/s20.img 300
  printf 'label: gpt\n, 96M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  mkfs.vfat -F 32 "${L}p1" >/dev/null 2>&1 || { no s20_mkfs "mkfs.vfat failed"; detach "$L"; rm -f /tmp/s20.img; return; }
  before=$(sfdisk -d "$L" 2>/dev/null)
  "$DE" resize "$L:1" --grow-to-end --yes >/tmp/o 2>&1; rc=$?
  assert_rc s20_refused 6 "$rc"
  grep -qi "unsupported" /tmp/o && ok s20_message || no s20_message "$(cat /tmp/o)"
  assert_eq s20_no_write "$before" "$(sfdisk -d "$L" 2>/dev/null)"
  mkdir -p /mnt/s20
  mount "${L}p1" /mnt/s20 2>/tmp/o && { ok s20_fs_intact; umount /mnt/s20; } || no s20_fs_intact "mount vfat failed"
  detach "$L"; rm -f /tmp/s20.img
}

# NTFS 离线收缩：真实 ntfsresize --size 把卷缩到分区末尾。
s21_ntfs_shrink() {
  newdisk /tmp/s21.img 300
  printf 'label: gpt\n, 200M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  mkntfs -q -F "${L}p1" >/dev/null 2>&1 || { no s21_mkfs "mkntfs failed"; detach "$L"; rm -f /tmp/s21.img; return; }
  size_of() { ntfsresize --info --force "$1" 2>&1 | grep 'Current volume size' | grep -o '[0-9][0-9]* bytes' | head -1 | cut -d' ' -f1; }
  before=$(size_of "${L}p1")
  "$DE" resize "$L:1" --end 198655 --yes >/tmp/o 2>&1; rc=$?
  assert_rc s21_shrink_rc 0 "$rc"
  after=$(size_of "${L}p1")
  assert_gt s21_volume_shrank "$before" "$after"
  detach "$L"; rm -f /tmp/s21.img
}

# btrfs 离线收缩：挂载后 btrfs filesystem resize 缩到目标，再落分区表。
s22_btrfs_shrink() {
  modprobe btrfs 2>/dev/null
  newdisk /tmp/s22.img 960
  printf 'label: gpt\n, 768M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  mkfs.btrfs -q -f "${L}p1" >/dev/null 2>&1 || { no s22_mkfs "mkfs.btrfs failed"; detach "$L"; rm -f /tmp/s22.img; return; }
  "$DE" resize "$L:1" --end 1050623 --yes >/tmp/o 2>&1; rc=$?
  assert_rc s22_shrink_rc 0 "$rc"
  dev=$(btrfs inspect-internal dump-super "${L}p1" 2>/dev/null | awk '$1=="total_bytes"{print $2}')
  part=$(bytes "${L}p1")
  assert_ge s22_btrfs_within_part "$part" "$dev"
  assert_gt s22_btrfs_shrank "600000000" "$dev"
  detach "$L"; rm -f /tmp/s22.img
}

# LVM 离线 PV 链：PV 扩容后按增量扩指定 LV（不动其内文件系统）。
s23_lvm_offline_chain() {
  newdisk /tmp/s23.img 600
  printf 'label: gpt\n, 300M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1
  pvcreate -y -ff "${L}p1" >/dev/null 2>&1 || { no s23_pvcreate "pvcreate failed"; detach "$L"; rm -f /tmp/s23.img; return; }
  vgcreate vgsa "${L}p1" >/tmp/o 2>&1 || { no s23_vgcreate "$(cat /tmp/o)"; detach "$L"; rm -f /tmp/s23.img; return; }
  lvcreate -y -L 64M -n lvsa vgsa --wipesignatures n -Z n >/dev/null 2>&1 || { no s23_lvcreate "lvcreate failed"; detach "$L"; rm -f /tmp/s23.img; return; }
  vgmknodes >/dev/null 2>&1
  mkfs.ext4 -q -F /dev/vgsa/lvsa
  pv_before=$(pvs --noheadings --units b --nosuffix -o pv_size "${L}p1" | tr -d ' ')
  lv_before=$(lvs --noheadings --units b --nosuffix -o lv_size vgsa/lvsa | tr -d ' ')
  "$DE" resize "$L:1" --grow-to-end --grow-lv --yes >/tmp/o 2>&1; rc=$?
  assert_rc s23_rc 0 "$rc"
  pv_after=$(pvs --noheadings --units b --nosuffix -o pv_size "${L}p1" | tr -d ' ')
  lv_after=$(lvs --noheadings --units b --nosuffix -o lv_size vgsa/lvsa | tr -d ' ')
  assert_gt s23_pv_grew "$pv_after" "$pv_before"
  assert_gt s23_lv_grew "$lv_after" "$lv_before"
  e2fsck -fn /dev/vgsa/lvsa >/dev/null 2>&1; assert_rc s23_fs_clean 0 "$?"
  detach "$L"; vgremove -ff vgsa 2>/dev/null; pvremove -y "${L}p1" 2>/dev/null
  rm -f /tmp/s23.img
}

# S24: set label / set uuid 经真实工具写入并回读。
s24_set_label_uuid() {
  modprobe btrfs 2>/dev/null
  newdisk /tmp/s24.img 1000
  printf 'label: gpt\n, 128M, L\n, 64M, L\n, 96M, L\n, 64M, L\n, 64M, L\n, 320M, L\n, 160M, L\n' | sfdisk "$L" >/dev/null 2>&1; sleep 1

  mkfs.ext4 -q -F "${L}p1"
  "$DE" set "$L:1" label ROOTSA --yes >/tmp/o 2>&1; assert_rc s24_ext_label_rc 0 "$?"
  assert_eq s24_ext_label ROOTSA "$(blkid -o value -s LABEL "${L}p1" 2>/dev/null)"
  want=4d4f4e4b-0000-4000-8000-000000000001
  "$DE" set "$L:1" uuid "$want" --yes >/tmp/o 2>&1; assert_rc s24_ext_uuid_rc 0 "$?"
  assert_eq s24_ext_uuid "$want" "$(blkid -o value -s UUID "${L}p1" 2>/dev/null)"
  old=$(blkid -o value -s UUID "${L}p1" 2>/dev/null)
  "$DE" set "$L:1" uuid --random --yes >/tmp/o 2>&1; assert_rc s24_ext_random_rc 0 "$?"
  new=$(blkid -o value -s UUID "${L}p1" 2>/dev/null)
  { [ -n "$new" ] && [ "$new" != "$old" ]; } && ok s24_ext_random_changed || no s24_ext_random_changed "uuid stayed [$old]"

  mkfs.vfat -F 32 "${L}p2" >/dev/null 2>&1
  "$DE" set "$L:2" label VFLAB --yes >/tmp/o 2>&1; assert_rc s24_vfat_label_rc 0 "$?"
  assert_eq s24_vfat_label VFLAB "$(blkid -o value -s LABEL "${L}p2" 2>/dev/null)"
  "$DE" set "$L:2" uuid --random --yes >/tmp/o 2>&1
  assert_rc s24_vfat_uuid_refused 3 "$?"

  mkntfs -q -F "${L}p3" >/dev/null 2>&1
  "$DE" set "$L:3" label NLAB --yes >/tmp/o 2>&1; assert_rc s24_ntfs_label_rc 0 "$?"
  assert_eq s24_ntfs_label NLAB "$(blkid -o value -s LABEL "${L}p3" 2>/dev/null)"
  "$DE" set "$L:3" uuid --random --yes >/tmp/o 2>&1; assert_rc s24_ntfs_random_rc 0 "$?"
  "$DE" set "$L:3" uuid 11111111-1111-4111-8111-111111111111 --yes >/tmp/o 2>&1
  assert_rc s24_ntfs_explicit_refused 3 "$?"

  mkfs.exfat "${L}p4" >/dev/null 2>&1
  "$DE" set "$L:4" label ELAB --yes >/tmp/o 2>&1; assert_rc s24_exfat_label_rc 0 "$?"
  assert_eq s24_exfat_label ELAB "$(blkid -o value -s LABEL "${L}p4" 2>/dev/null)"

  mkswap "${L}p5" >/dev/null 2>&1
  want=22222222-2222-4222-8222-222222222222
  "$DE" set "$L:5" uuid "$want" --yes >/tmp/o 2>&1; assert_rc s24_swap_uuid_rc 0 "$?"
  swaplabel "${L}p5" 2>/dev/null | grep -q "$want" && ok s24_swap_uuid || no s24_swap_uuid "$(swaplabel "${L}p5" 2>&1)"

  mkfs.xfs -q -f "${L}p6" >/dev/null 2>&1
  "$DE" set "$L:6" label XLAB --yes >/tmp/o 2>&1; assert_rc s24_xfs_label_rc 0 "$?"
  assert_eq s24_xfs_label XLAB "$(blkid -o value -s LABEL "${L}p6" 2>/dev/null)"

  mkfs.btrfs -q -f "${L}p7" >/dev/null 2>&1
  "$DE" set "$L:7" label BLAB --yes >/tmp/o 2>&1; assert_rc s24_btrfs_label_rc 0 "$?"
  assert_eq s24_btrfs_label BLAB "$(blkid -o value -s LABEL "${L}p7" 2>/dev/null)"

  "$DE" records show "$L" >/tmp/o 2>&1
  grep -q '^record: absent' /tmp/o && ok s24_no_pending_record || no s24_no_pending_record "$(cat /tmp/o)"
  detach "$L"; rm -f /tmp/s24.img
}

echo "=== SCENARIOS START ==="
s1_offline_ext4_grow
s2_offline_ext4_shrink
s3_mbr_ext4_grow
s4_btrfs_grow
s5_online_ext4_grow
s6_swap_grow
s7_lvm_online_pv_grow
s8_move_preserves_data
s9_alias_refused
s10_table_edits
s11_mkfs_and_check
s12_online_xfs_grow
s13_vfat_mkfs
s14_multidevice_btrfs_refused
s15_ntfs_mkfs_check
s16_exfat_mkfs_check
s17_f2fs_mkfs_check
s18_ntfs_grow
s19_f2fs_grow
s20_vfat_grow_refused
s21_ntfs_shrink
s22_btrfs_shrink
s23_lvm_offline_chain
s24_set_label_uuid
echo "=== SCENARIOS SUMMARY pass=$PASS fail=$FAIL failed=[$FAILED] ==="
"##;

/// 来宾需要的工具集：分区、文件系统、LVM、内核模块与探测。
const PACKAGES: &str = "lvm2,btrfs-progs,xfsprogs,dosfstools,exfatprogs,ntfs-3g,f2fs-tools,fatresize,kmod,\
e2fsprogs,util-linux,procps,pciutils,fdisk";

#[test]
fn real_kernel_behaviour_suite() {
    if std::env::var("DISKEDIT_QEMU").as_deref() != Ok("1") {
        eprintln!(
            "NoEnvironment: set DISKEDIT_QEMU=1 to run the real-kernel suite in QEMU \
             (needs qemu-system-x86_64 and a kernel image)"
        );
        return;
    }
    let cache = env_path("DISKEDIT_QEMU_CACHE")
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target/qemu"));
    let kernel = env_path("DISKEDIT_QEMU_KERNEL")
        .or_else(default_kernel)
        .unwrap_or_else(|| {
            panic!(
                "NoEnvironment: no bootable kernel image found in /boot; set DISKEDIT_QEMU_KERNEL"
            )
        });
    assert!(
        kernel.is_file(),
        "NoEnvironment: kernel image {} is missing; set DISKEDIT_QEMU_KERNEL",
        kernel.display()
    );
    let release = kernel_release(&kernel);
    let rootfs = prepare_rootfs(&cache, &release);
    stage_guest(&rootfs);

    let image = cache.join("rootfs.img");
    build_image(&rootfs, &image);
    let log = cache.join("vm.log");
    boot(&kernel, &image, &log);
    let text = std::fs::read_to_string(&log).expect("读回来宾串口日志");
    assert!(
        text.contains("=== VM_RESULT_DONE ==="),
        "the guest did not finish; see {}\n{}",
        log.display(),
        tail(&text, 40)
    );
    let failures = text
        .lines()
        .filter(|line| line.starts_with("FAIL "))
        .collect::<Vec<_>>();
    let passes = text
        .lines()
        .filter(|line| line.starts_with("PASS "))
        .count();
    for line in text.lines().filter(|line| line.starts_with("FAIL ")) {
        eprintln!("{line}");
    }
    assert!(
        failures.is_empty(),
        "{} of {} real-kernel checks failed; see {}",
        failures.len(),
        passes + failures.len(),
        log.display()
    );
    assert!(passes > 0, "no checks ran; see {}", log.display());
    eprintln!("real-kernel suite: {passes} checks passed");
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// 默认来宾内核：优先当前运行的版本，否则取 `/boot` 里有对应模块树的最新一版。
/// 沙箱里 `uname -r` 可能与 `/boot` 里的镜像不一致，只认有模块树的那一个。
fn default_kernel() -> Option<PathBuf> {
    let running = PathBuf::from(format!(
        "/boot/vmlinuz-{}",
        command_text("uname", &["-r"]).trim()
    ));
    if running.is_file() {
        return Some(running);
    }
    let mut candidates: Vec<(String, PathBuf)> = std::fs::read_dir("/boot")
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let release = name.strip_prefix("vmlinuz-")?.to_owned();
            Path::new("/lib/modules")
                .join(&release)
                .is_dir()
                .then_some((name, entry.path()))
        })
        .collect();
    candidates.sort();
    candidates.pop().map(|(_, path)| path)
}

/// 从镜像文件名取出内核版本：`/boot/vmlinuz-6.8.0-151-generic` -> `6.8.0-151-generic`。
fn kernel_release(kernel: &Path) -> String {
    kernel
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("vmlinuz-"))
        .unwrap_or_else(|| panic!("内核镜像名不是 vmlinuz-<release>：{}", kernel.display()))
        .to_owned()
}

fn command_text(program: &str, args: &[&str]) -> String {
    String::from_utf8_lossy(
        &Command::new(program)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("{program}: {error}"))
            .stdout,
    )
    .into_owned()
}

/// 复用用户给的 rootfs，或在缓存目录里 debootstrap 一份。构建结果带标记，避免重复联网。
fn prepare_rootfs(cache: &Path, release: &str) -> PathBuf {
    if let Some(existing) = env_path("DISKEDIT_QEMU_ROOTFS") {
        return existing;
    }
    let rootfs = cache.join("rootfs");
    let marker = cache.join("rootfs.ready");
    if marker.is_file() {
        return rootfs;
    }
    std::fs::create_dir_all(cache).expect("创建缓存目录");
    if rootfs.exists() {
        std::fs::remove_dir_all(&rootfs).expect("清理旧 rootfs");
    }
    let mirror = std::env::var("DISKEDIT_QEMU_MIRROR")
        .unwrap_or_else(|_| "http://mirrors.volces.com/ubuntu".to_owned());
    let status = Command::new("debootstrap")
        .args([
            "--variant=minbase",
            "--components=main,universe",
            "--include",
            PACKAGES,
            "--arch=amd64",
            "noble",
            rootfs.to_str().expect("rootfs 路径可表示"),
            &mirror,
        ])
        .status()
        .expect("运行 debootstrap（需 root 与网络；也可用 DISKEDIT_QEMU_ROOTFS 复用）");
    assert!(status.success(), "debootstrap 失败");
    let release = release.to_owned();
    let modules = Path::new("/lib/modules").join(release.trim());
    assert!(
        modules.is_dir(),
        "NoEnvironment: kernel modules {} are missing; btrfs/xfs would not load",
        modules.display()
    );
    let target_modules = rootfs.join("lib/modules");
    std::fs::create_dir_all(&target_modules).expect("创建模块目录");
    let status = Command::new("cp")
        .arg("-a")
        .arg(&modules)
        .arg(&target_modules)
        .status()
        .expect("复制内核模块");
    assert!(status.success(), "复制内核模块失败");
    std::fs::write(&marker, b"ok\n").expect("写标记");
    rootfs
}

/// 把被测二进制与两份来宾脚本放进 rootfs。
fn stage_guest(rootfs: &Path) {
    let binary = Path::new(env!("CARGO_BIN_EXE_DiskEdit"));
    let root = rootfs.join("root");
    std::fs::create_dir_all(&root).expect("创建 /root");
    std::fs::copy(binary, root.join("diskedit")).expect("复制二进制");
    std::fs::write(root.join("scenarios.sh"), SCENARIOS).expect("写场景脚本");
    std::fs::write(rootfs.join("init"), INIT).expect("写 init");
    for path in [
        root.join("diskedit"),
        root.join("scenarios.sh"),
        rootfs.join("init"),
    ] {
        let status = Command::new("chmod")
            .arg("+x")
            .arg(&path)
            .status()
            .expect("chmod");
        assert!(status.success(), "chmod {} 失败", path.display());
    }
}

fn build_image(rootfs: &Path, image: &Path) {
    if image.exists() {
        std::fs::remove_file(image).expect("删除旧镜像");
    }
    let status = Command::new("truncate")
        .args(["-s", "2G"])
        .arg(image)
        .status()
        .expect("运行 truncate");
    assert!(status.success(), "创建 rootfs 镜像失败");
    let status = Command::new("mkfs.ext4")
        .args(["-q", "-F", "-d"])
        .arg(rootfs)
        .arg(image)
        .status()
        .expect("运行 mkfs.ext4");
    assert!(status.success(), "构建 rootfs 镜像失败");
}

fn boot(kernel: &Path, image: &Path, log: &Path) {
    let accel = if Path::new("/dev/kvm").exists() {
        "kvm"
    } else {
        "tcg,thread=multi"
    };
    let status = Command::new("qemu-system-x86_64")
        .args(["-m", "2048", "-smp", "2", "-accel", accel])
        .arg("-kernel")
        .arg(kernel)
        .args([
            "-append",
            "root=/dev/vda rw console=ttyS0 quiet init=/init oops=panic panic=-1",
            "-drive",
        ])
        .arg(format!(
            "file={},format=raw,if=virtio,cache=unsafe",
            image.display()
        ))
        .args([
            "-display",
            "none",
            "-monitor",
            "none",
            "-no-reboot",
            "-serial",
        ])
        .arg(format!("file:{}", log.display()))
        .status()
        .expect("运行 qemu-system-x86_64");
    assert!(status.success(), "qemu 退出码非零");
}

fn tail(text: &str, lines: usize) -> String {
    let all = text.lines().collect::<Vec<_>>();
    all[all.len().saturating_sub(lines)..].join("\n")
}
