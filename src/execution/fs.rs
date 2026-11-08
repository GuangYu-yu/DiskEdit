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
    let (base, len) = step
        .byte_range(sector_size)
        .map_err(|why| RunOutcome::internal(format!("{why} — nothing was written")))?;
    let growable = fsops::grow_target_at(src, step.part, base, len).map_err(RunOutcome::from)?;
    if let Some(planned) = step.fstype
        && let Growable::Target(target) = &growable
        && target.fstype != planned
    {
        return Err(RunOutcome::refused(format!(
            "partition {} holds {} on disk but the plan was made for {planned} — nothing was written",
            step.part, target.fstype
        )));
    }
    fsops::check_grow_step(growable, src.is_block).map_err(RunOutcome::from)
}

/// 执行一个 FsResize 步骤。
///
/// `wrote_target` 是进入本步时计划是否已经写过目标字节：只有它为 false 时，FS 层的
/// 失败才能按 [`RunOutcome::from`] 的原分类上报。越过首写之后一律改判为
/// [`RunOutcome::partial`]，否则 [`FsError::Io`] 会把"可能已写"洗成"确定未写盘"。
///
/// `Ok(Some(pending))` 是本步**做过一部分、后置条件未达成**：表已扩而文件系统侧没有
/// 可写的动作，或外部工具失败。它与失败的区别是"盘上的状态是已知的、还差一步"，
/// 因此由调用方带进结果而不是当成错误——扩容收尾的分类只有一份（`movepart::grow_post`）。
pub(crate) fn execute(
    src: &mut WriteSource,
    step: &FsResize,
    sector_size: u64,
    wrote_target: bool,
    sink: &mut dyn StepSink,
) -> Result<Option<Pending>, RunOutcome> {
    let (base, len) = step.byte_range(sector_size).map_err(|why| {
        // 预扫描已经判过同一份载荷，这里的失败只是兜底：首写之前仍是计划不合法，
        // 首写之后按"盘可能已改变"上报
        if wrote_target {
            RunOutcome::partial(why)
        } else {
            RunOutcome::internal(why)
        }
    })?;
    let growable = fsops::grow_target_at(src, step.part, base, len)
        .map_err(|e| fs_failure(e, step.part, wrote_target))?;
    let target = match growable {
        Growable::Target(target) => target,
        // 预扫描按同一份结论放行过，走到这里说明判定与执行之间盘被改过。
        // 已写过目标字节：表已扩、文件系统侧没有可写的动作——用与命令级收尾同一份分类
        // 交出一条待办；没写过：仍是"本步没有可做的事"，按拒绝上报
        other => {
            if wrote_target {
                let p = crate::movepart::grow_post(step.part, &other);
                sink.note(&p.log);
                return Ok(p.pending);
            }
            return Err(unfinished(false, no_target_reason(step.part, &other)));
        }
    };
    // 外部工具改写的字节不进 journal：屏障落在工具启动之前，undo 见屏障即拒绝回滚
    src.mark_non_reversible().map_err(|e| {
        let cause = format!(
            "partition {}: cannot persist the recovery barrier before resizing the filesystem: {e}",
            step.part
        );
        if wrote_target {
            RunOutcome::partial(cause)
        } else {
            RunOutcome::no_mutation(cause)
        }
    })?;
    let hint = || {
        crate::fsops::rescue_hint(
            Some(target.fstype),
            &crate::dev::part_dev_hint(src, step.part, base),
        )
    };
    match target.resize_fs(src) {
        // 核心已完成；挂载点没卸掉是一条未完成的后置条件，不是本步失败
        Ok(Some(leak)) => Ok(Some(leak.into_pending(step.part))),
        Ok(None) => Ok(None),
        // 写过目标字节：盘上的状态是已知的（表已扩、FS 未扩），补救命令由 FS 层给
        Err(e) if wrote_target => Ok(Some(Pending::new(
            step.part,
            PendingKind::Fs,
            e.to_string(),
            hint(),
        ))),
        Err(e) => Err(fs_failure(e, step.part, false)),
    }
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

/// FS 层失败落成出口语义。越过首写之后只有一种事实可说：盘可能已改变
fn fs_failure(e: FsError, part: u32, wrote_target: bool) -> RunOutcome {
    if wrote_target {
        RunOutcome::partial(format!(
            "partition {part}: the filesystem resize failed: {e}"
        ))
    } else {
        RunOutcome::from(e).context(&format!("partition {part}: the filesystem resize failed"))
    }
}

/// 本步没有做成、也没有产生本步的写入：已写过目标字节时归部分完成，
/// 否则按请求与现状不符拒绝
fn unfinished(wrote_target: bool, cause: impl Into<String>) -> RunOutcome {
    if wrote_target {
        RunOutcome::partial(cause)
    } else {
        RunOutcome::refused(cause)
    }
}
