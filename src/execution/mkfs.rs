//! FsCreate 步骤：在分区上建立文件系统。
//!
//! 载荷自洽在首写前判定；建立本身仍走 [`fsops::mkfs`]——屏障、残留签名擦除、参数拼装
//! 与工具调用都在那里，本层只把计划里的几何与类型取出来交给它，不另落一次屏障。

use crate::dev::WriteSource;
use crate::fsops::{self, MkfsFs};
use crate::outcome::RunOutcome;

/// 一个 FsCreate 步骤：字段与 [`crate::planner::PlanStep::FsCreate`] 一一对应。
///
/// `fstype` 是请求本身的类型，ext2/3/4 不折叠；`label` 原样作为工具参数，`None` 即不传。
pub(crate) struct FsCreate {
    pub part: u32,
    pub first_lba: u64,
    pub sectors: u64,
    pub fstype: MkfsFs,
    pub label: Option<String>,
}

impl FsCreate {
    /// 载荷换算成字节区间。零长度、长度溢出、起点溢出都说明计划没有经过装配
    fn byte_range(&self, sector_size: u64) -> Result<(u64, u64), String> {
        if self.sectors == 0 {
            return Err(format!(
                "partition {}: the create step covers zero sectors",
                self.part
            ));
        }
        let len = self.sectors.checked_mul(sector_size).ok_or_else(|| {
            format!(
                "partition {}: {} sectors of {sector_size} bytes overflow the address space",
                self.part, self.sectors
            )
        })?;
        let base = self.first_lba.checked_mul(sector_size).ok_or_else(|| {
            format!(
                "partition {}: start LBA {} overflows the address space",
                self.part, self.first_lba
            )
        })?;
        if base.checked_add(len).is_none() {
            return Err(format!(
                "partition {}: byte range {base}+{len} overflows the address space",
                self.part
            ));
        }
        Ok((base, len))
    }
}

/// 首次写入前的预扫描：载荷自洽、区间在盘内、类型能得到具体的工具。
///
/// 尚未写过目标字节，失败按 [`RunOutcome::from`] 的原分类上报（类型不成立 → 6）。
/// 工具是否在 PATH 里、卷标长度、平台与权限、占用闸都不在这里：它们属于命令层在开事务
/// 之前的准入判定，执行器重复一遍只会出现两份措辞。
pub(crate) fn preflight(
    src: &WriteSource,
    step: &FsCreate,
    sector_size: u64,
) -> Result<(), RunOutcome> {
    let (base, len) = step.byte_range(sector_size).map_err(RunOutcome::internal)?;
    if base > src.size || len > src.size - base {
        return Err(RunOutcome::internal(format!(
            "partition {}: the {len}-byte range at byte {base} is outside the target's {} bytes",
            step.part, src.size
        )));
    }
    fsops::mkfs_tool(step.fstype).map_err(RunOutcome::from)?;
    Ok(())
}

/// 执行一个 FsCreate 步骤。
///
/// 失败只报 `fsops` 的原因与本次在做什么（[`RunOutcome::from`] 的原分类）：本进程是否
/// 已经写过目标字节由出口的归责点判，擦除前的 IO 失败不会把"可能已写"洗成"确定未写盘"。
pub(crate) fn execute(
    src: &mut WriteSource,
    step: &FsCreate,
    sector_size: u64,
) -> Result<(), RunOutcome> {
    // 预扫描已经判过同一份载荷，这里的失败只是兜底：计划不合法
    step.byte_range(sector_size).map_err(RunOutcome::internal)?;
    fsops::mkfs(src, step.part, step.fstype, step.label.as_deref()).map_err(|e| {
        RunOutcome::from(e)
            .context(&format!("partition {}: creating the filesystem failed", step.part))
    })
}
