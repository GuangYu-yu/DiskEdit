//! FsResize 步骤：把分区里的文件系统改写到分区的当前尺寸。
//!
//! 判定与执行同源：区间先交给 [`fsops::grow_target_at`]，再由
//! [`fsops::GrowTarget::resize_fs`] 在同一份结论上执行。本层不推算 overlay 偏移，
//! 也不另列一份可扩类型的清单。

use crate::dev::WriteSource;
use crate::domain::ContentKind;
use crate::fsops::{self, FsError, Growable};
use crate::outcome::{Pending, PendingKind, RunOutcome};

use super::StepSink;

/// 一个 FsResize 步骤：字段与 [`crate::planner::PlanStep::FsResize`] 一一对应。
///
/// `fstype` 是计划期钉住的类型；装配层不做探测，产出的计划恒为 `None`，预扫描按
/// 区间识别的结论执行。带值时只用于核对盘上类型与计划是否一致。
pub(crate) struct FsResize {
    pub part: u32,
    pub first_lba: u64,
    pub sectors: u64,
    pub target_bytes: u64,
    pub fstype: Option<ContentKind>,
}

impl FsResize {
    /// 载荷换算成字节区间。任一不一致都说明计划没有经过装配：长度溢出、
    /// 记录的字节数与扇区长度不符、起点越界
    fn byte_range(&self, sector_size: u64) -> Result<(u64, u64), String> {
        let len = self.sectors.checked_mul(sector_size).ok_or_else(|| {
            format!(
                "partition {}: {} sectors of {sector_size} bytes overflow the address space",
                self.part, self.sectors
            )
        })?;
        if len != self.target_bytes {
            return Err(format!(
                "partition {}: the step's {len}-byte range does not match its recorded {} target bytes",
                self.part, self.target_bytes
            ));
        }
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

/// 首次写入前的预扫描：载荷自洽，且这段区域里的文件系统在本目标上扩得了。
///
/// 尚未写过目标字节，失败按 [`RunOutcome::from`] 的原分类上报（能力缺口 → 6）。
pub(crate) fn preflight(
    src: &WriteSource,
    step: &FsResize,
    sector_size: u64,
) -> Result<(), RunOutcome> {
    let (base, len) = step.byte_range(sector_size).map_err(RunOutcome::internal)?;
    let growable = fsops::grow_target_at(src, step.part, base, len).map_err(RunOutcome::from)?;
    if let Some(planned) = step.fstype
        && let Growable::Target(target) = &growable
        && target.fstype != planned
    {
        return Err(RunOutcome::refused(format!(
            "partition {} holds {} on disk but the plan was made for {planned}",
            step.part, target.fstype
        )));
    }
    fsops::check_grow_step(growable, src.is_block).map_err(RunOutcome::from)
}

/// 执行一个 FsResize 步骤。
///
/// 恢复屏障由 [`fsops::GrowTarget::resize_fs`] 在设备建立之后、第一个工具启动之前落下。
/// 屏障之前的失败按 [`RunOutcome::from`] 的原分类上报；工具启动之后的失败由
/// [`crate::outcome::crossed_irreversible_point`] 交成待办。
///
/// `Ok(Some(pending))` 是本步**做过一部分、后置条件未达成**：表已扩而文件系统侧没有
/// 可写的动作，或外部工具失败。它与失败的区别是"盘上的状态是已知的、还差一步"，
/// 因此由调用方带进结果而不是当成错误——扩容收尾的分类只有一份（`movepart::grow_post`）。
pub(crate) fn execute(
    src: &mut WriteSource,
    step: &FsResize,
    sector_size: u64,
    sink: &mut dyn StepSink,
) -> Result<Option<Pending>, RunOutcome> {
    // 预扫描已经判过同一份载荷，这里的失败只是兜底：计划不合法
    let (base, len) = step.byte_range(sector_size).map_err(RunOutcome::internal)?;
    let growable =
        fsops::grow_target_at(src, step.part, base, len).map_err(|e| fs_failure(e, step.part))?;
    let target = match growable {
        Growable::Target(target) => target,
        // 预扫描按同一份结论放行过，走到这里说明判定与执行之间盘被改过。
        // 判据：表已扩（越过不可逆写入点）时，这条事实只能作为待办交出；
        // 未越过的仍是"本步没有可做的事"，按拒绝上报
        other => {
            return crate::outcome::attribute(
                || Ok(crate::movepart::grow_post(step.part, &other)),
                || Err(RunOutcome::refused(no_target_reason(step.part, &other))),
            )
            .map(|p| {
                sink.note(&p.log);
                p.pending
            });
        }
    };
    // 补救文案的两份材料在写句柄借用之前取好：`resize_fs` 需要 `&mut`，闭包持有 `&src`
    // 会与它冲突
    let fstype = target.fstype;
    let dev = crate::dev::part_dev_hint(src, step.part, base);
    match target.resize_fs(src) {
        // 核心已完成；挂载点没卸掉是一条未完成的后置条件，不是本步失败
        Ok(Some(leak)) => Ok(Some(leak.into_pending(step.part))),
        Ok(None) => Ok(None),
        // 工具已启动：盘上状态不再可断言，交成待办，补救命令由 FS 层给
        Err(e) if started_tool(&e) => Ok(Some(Pending::new(
            step.part,
            PendingKind::Fs,
            e.to_string(),
            crate::fsops::rescue_hint(Some(fstype), &dev, e.unsupported_reason()),
        ))),
        // 工具没起来、或在启动之前就被拒：目标字节没有被写，按原分类交出。屏障已落下的
        // 此类失败由出口的归责改判为部分完成
        Err(e) => Err(fs_failure(e, step.part)),
    }
}

/// 失败是否以外部工具已启动为前提。`CommandFailed` 来自非零退出或等待失败；其余变体
/// （能力缺口、前置拒绝、工具缺失、启动前的 I/O 失败）都没有写过目标字节
fn started_tool(e: &fsops::FsError) -> bool {
    matches!(e, fsops::FsError::CommandFailed(_))
}

/// 首写之前看到"没有可扩的目标"时的措辞：说本步没做成，不说"扩了但没做完"。
/// 已写之后的同类状态由 `movepart::grow_post` 负责
fn no_target_reason(part: u32, g: &Growable) -> String {
    match g {
        Growable::OverlayPending => format!(
            "partition {part}: the trailing overlay layer holds no identifiable filesystem — nothing to resize"
        ),
        Growable::SwapUnactivatable => format!(
            "partition {part}: the swap area cannot be activated on this host — rebuild it with mkswap instead of resizing"
        ),
        Growable::NoFilesystem(kind) => format!(
            "partition {part}: no growable filesystem was identified (identified as `{}`)",
            crate::fsid::content_name(kind.unwrap_or(ContentKind::Unknown))
        ),
        // 上面的 match 已把 Target 接走
        Growable::Target(t) => format!(
            "partition {part}: the {} inside was not resized — its type changed between validation and execution",
            t.fstype
        ),
    }
}

/// FS 层失败落成出口语义：原因按 [`RunOutcome::from`] 的原分类交出，
/// 已写过目标字节时由出口的归责改判为部分完成
fn fs_failure(e: FsError, part: u32) -> RunOutcome {
    RunOutcome::from(e).context(&format!("partition {part}: the filesystem resize failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_started_tool_is_folded_into_a_pending() {
        // 进程已启动：非零退出与等待失败都在这里
        assert!(started_tool(&FsError::CommandFailed("resize2fs exited 1".into())));
        // 启动之前：能力缺口、前置拒绝、工具缺失、启动前的 I/O 失败
        assert!(!started_tool(&FsError::unsupported_multi_device_btrfs("spans multiple devices")));
        assert!(!started_tool(&FsError::platform("not linux")));
        assert!(!started_tool(&FsError::invalid("bad range")));
        assert!(!started_tool(&FsError::missing("required tool not found: e2fsck")));
        assert!(!started_tool(&FsError::Io(std::io::Error::other("read failed"))));
    }
}
