//! 在线 resize 的公开入口。

use super::*;

/// 在线 resize 总入口（resizefs --online 用）。`size`：None = 扩满现分区；Some(字节) = 绝对新尺寸
/// （> 现分区与 < 现分区都被拒绝：本入口不改分区表，缩容需要分区跟着缩，属 `resize` 的职责）。
/// 请求不含锚定语义，故目标在锁下原样给出。
/// 结果统一为 Outcome（退出码只在 outcome 模块映射）：10=拒绝（未动盘）/
/// 20=布局已写但后置条件或内核视图未跟上 / 30=执行失败
pub fn resize_online(mountpoint: &Path, size: Option<u64>) -> Outcome {
    flow::resize_online(mountpoint, move |_| Ok(size), flow::TablePolicy::FilesystemOnly)
}

/// 在线 resize（resize 命令用）：目标长度由 `decide` 在**持有目标锁之后**、以只读打开的
/// 整盘为输入解析——`+N`/`+N%` 锚定"当前尺寸"、`grow` 锚定右侧空闲，锁前快照在窗口内可能已作废。
/// `Ok(None)` = 不指定绝对尺寸（仅把 FS 扩满现分区）；`Ok(Some(b))` = 绝对新尺寸，
/// 等于现分区长度时不写入任何东西（与离线路径的"尺寸未变"同一判据）；
/// 大于现分区时允许先扩分区表（本入口的职责包含分区几何）
pub fn resize_online_planned(
    mountpoint: &Path,
    decide: impl FnOnce(&FileSource) -> Result<Option<u64>, Fail>,
) -> Outcome {
    flow::resize_online(mountpoint, decide, flow::TablePolicy::WithTable)
}

/// PV 在线扩容（仅分区层，无 FS 层）：sfdisk 写表 → partx 同步（BLKPG 兜底）→
/// 尺寸核验。PV 本身不挂载，但活动 LV 经 device-mapper 持有分区使 BLKRRPART
/// EBUSY，因此必须走与挂载分区相同的 partx/BLKPG 同步路径；pvresize/lvextend
/// 由调用方在分区尺寸生效后执行（pvresize(8) 支持已属 VG 且有活动 LV 的 PV）。
/// 目标长度同样由 `decide` 在锁下解析，`Ok(None)` 视作"无绝对目标"（分区层无变化）。
/// 返回 (结果, 锁下取得的实际旧尺寸, 独占锁与锁下的整盘句柄)：调用方计算扩量只能用第二个值——
/// 锁前快照在窗口内可能已过期，用它算出的扩量会传导给 lvextend。前置失败时旧尺寸带 0、
/// 第三项为 None，调用方只在 complete 分支消费；complete 分支的锁必须持有到 PV 链结束，
/// 收尾读表也只用那里带出的句柄
pub fn resize_pv_online(
    disk_name: &str,
    pno: u32,
    decide: impl FnOnce(&FileSource) -> Result<Option<u64>, Fail>,
) -> (Outcome, u64, Option<pv::LockedPv>) {
    resize_pv(disk_name, pno, decide)
}