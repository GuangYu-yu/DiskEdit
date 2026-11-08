//! 输入源：镜像文件与块设备统一为按偏移读写的字节存储。
//!
//! 本模块实际承载四件事（模块文档即职责表，改职责先改这里）：
//! 1. **输入源**（`FileSource`）：按偏移的 read/sync，镜像与块设备同一接口；
//!    **写句柄**（`WriteSource`）：在已打开的目标上持有独占所有权，写方法只挂在它上面；
//! 2. **身份落点**（`TargetIdentity`）：设备层拓扑 → record / log / lock 的唯一派生处；
//! 3. **Disk GUID 凭据**（`DiskGuidState`）：目标盘面自述身份的编码与比较；
//! 4. **losetup 用户提示**（`part_dev_hint`）：镜像上定位分区的外部工具提示。
//!
//! `TargetIdentity` 经两个具名构造取得：`resolve_image(path)` 恒成功；
//! `resolve_block(path)` 解析不出设备拓扑即失败——块设备身份拒绝任何退化路径。
//!
//! 身份一律是**盘级**的：分区号属于操作与校验，不进落盘键。loop 设备的身份是
//! backing 文件的 realpath（Linux；`/dev/loop0:2` 与直接以该镜像为目标的操作由此
//! 收敛到同一序列化域），非 loop 块设备仍是 sysfs 拓扑键。镜像身份以**已打开句柄**
//! 的路径为准（Linux：`/proc/self/fd`，块设备则按 fd 的 rdev 反查 sysfs），不从命令行
//! 路径二次取事实——打开与 canonicalize 之间的替换窗口会让身份与写句柄分叉。两个列表
//! 各只有一个落点（写入位置），由身份派生；发现后仍走完整校验，不做文件改名迁移。

mod source;
mod topology;
mod identity;
mod host_repo;
mod discovery;
mod occupancy;
mod target;
mod disk_guid;

#[cfg(test)]
mod tests;

pub(crate) use source::*;
pub(crate) use topology::*;
pub(crate) use identity::*;
pub(crate) use host_repo::*;
pub(crate) use discovery::*;
pub(crate) use occupancy::*;
pub(crate) use target::*;
pub(crate) use disk_guid::*;

pub(crate) use crate::recovery::RecordJournal;

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
#[cfg(target_os = "linux")]
use crate::ioctl;