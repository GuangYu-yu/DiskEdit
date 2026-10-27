//! 在线 resize（仅 Linux，须 root）：对挂载中的分区做持久化分区表缩放 + FS 工具扩缩。
//!
//! 两条路径（顺序依据 btrfs-filesystem(8)：先扩分区再扩 FS，先缩 FS 再缩分区）：
//! - grow：sfdisk 写表（持久化；GPT backup header 由 libfdisk 在 write 时修正到盘尾）→
//!   partx -u 同步内核 → FS 工具（ext=resize2fs 挂载设备 / xfs=xfs_growfs 挂载点 /
//!   btrfs=btrfs filesystem resize max 挂载点）
//! - shrink：仅 btrfs（SUSE 存储指南在线矩阵中唯一支持在线缩的 FS）→ FS 先缩 →
//!   sfdisk 写表 → partx -u
//!
//! 必须写盘上表而非纯 BLKPG：BLKPG_RESIZE_PARTITION 只改内核内存中的 bdev size
//! （resizepart(8)："doesn't manipulate partitions on a block device"），盘上表不更新，
//! 重启后分区回缩而 FS 已扩大。故先写盘上表，再让内核同步（partx -u；失败时退回
//! BLKPG ioctl 兜底）。
//!
//! 无 journal/checkpoint：在线场景内核与 FS 是共同写者，崩溃一致性归它们。
//! UAPI 依据：
//! include/uapi/linux/blkpg.h（BLKPG=_IO(0x12,105)、RESIZE=3、start/length 字节、pno 标识、
//! devname/volname 忽略）；block/ioctl.c（BLKPG 分支）：无 CAP_SYS_ADMIN → -EACCES；
//! 对分区 fd 调用、pno ≤ 0、range 非法/溢出、未按逻辑块对齐、超出盘容量 → -EINVAL，
//! 通过后才进入 BLKPG_ADD_PARTITION / BLKPG_RESIZE_PARTITION；内核不检查分区内 FS 占用。
//! block/partitions/core.c bdev_resize_partition：start 必须等于现值、相邻分区重叠 -EBUSY、
//! bdev_set_nr_sectors 对挂载中的分区立即生效。
//!
//! 在线能力矩阵（SUSE 存储指南 + 各工具 man）：ext2/3/4 在线仅 grow、xfs 仅 grow、
//! btrfs grow+shrink；ntfs/vfat/exfat/f2fs 工具要求未挂载，一律拒绝。
//!
//! btrfs 的挂载身份不取自 mountinfo 的设备号（内核为 btrfs 超级块分配 anon bdev），
//! 而取自 `BTRFS_IOC_FS_INFO` 的 FSID + `/sys/fs/btrfs/<FSID>/devices/`（Linux 5.6 起）
//! 与目标节点 st_rdev 的比对（见 target 模块的 `verify_btrfs_device`）。内核没有提供该
//! sysfs 映射、或映射与 ioctl 报的设备数不符时，btrfs 在线扩缩容一律拒绝：不退化成
//! "路径相同就算同一设备"

mod guard;
mod target;
mod part;
mod fs_grow;
mod pv;
mod faults;
mod flow;
mod api;

#[cfg(test)]
mod tests;

pub(crate) use guard::*;
pub(crate) use target::*;
pub(crate) use part::*;
pub(crate) use fs_grow::*;
pub(crate) use pv::*;
pub(crate) use faults::*;
pub(crate) use api::*;

use crate::dev::FileSource;
use crate::outcome::{Fail, Outcome, Pending, PendingKind};
use std::fs;
use std::io;
use std::os::linux::fs::MetadataExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};