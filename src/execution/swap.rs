//! 换区重建步骤：表项已提交到新落点之后，按旧头里的 UUID/卷标重新 mkswap。
//!
//! 换区的内容可弃，所以搬移一行字节也不读：那一条搬移的唯一持久效果是表项落位，
//! 重建才是让新位置可用的后置条件。它是本工具承诺过的语义——`grow_support` 对
//! swap 承诺的就是重建，漏掉它等于分区扩了而换区仍指向旧尺寸。

use crate::dev::WriteSource;
use crate::domain::ContentKind;
use crate::outcome::{Pending, PendingKind, RunOutcome};

use super::StepSink;

/// 一个换区重建步骤：分区号、旧起点（旧签名仍在源位）、新起点与长度
pub(crate) struct SwapRebuild {
    pub part: u32,
    pub src_first: u64,
    pub dst_first: u64,
    pub sectors: u64,
}

/// 在表项已提交之后重建换区。
///
/// 身份取 [`crate::movepart::swap_identity_preferring`] 的结论：旧位优先，旧位取不到
/// UUID 时回退到新位（自重叠搬移会覆盖旧头所在的那段）。
///
/// 返回 `Ok(Some(pending))` 是"后置条件未达成"——表已落位、内容可弃，记录保留，
/// 重跑从本条目重试；返回 `Err` 只留给"连屏障都没落成"这一类，那之后不能再写盘。
pub(crate) fn rebuild(
    src: &mut WriteSource,
    step: &SwapRebuild,
    sector_size: u64,
    sink: &mut dyn StepSink,
) -> Result<Option<Pending>, RunOutcome> {
    // mkswap 是外部工具的写盘，屏障必须先于它：此后回滚表项即"表与内容自相矛盾"
    src.mark_non_reversible().map_err(|e| {
        RunOutcome::partial(format!(
            "partition {}: {} — the table entry is committed",
            step.part,
            e.record_failure("the recovery barrier")
        ))
    })?;
    // 新起点此刻只在盘上的表里：内核视图若还停在旧位置，按新起点找不到分区节点，
    // mkswap 会以"表已提交"的状态失败成待办。重读的成败不改变本步的收场，只提示重跑
    if !crate::support::kernel_resync(src) {
        sink.note(
            "note: the kernel partition view did not follow the table write — the swap rebuild may need a re-run after the kernel re-reads the table",
        );
    }
    let ident = crate::movepart::swap_identity_preferring(
        src,
        step.src_first,
        step.dst_first,
        step.sectors,
        sector_size,
    );
    match crate::fsops::recreate_swap(src, step.part, &ident) {
        Ok(note) => {
            sink.note(&format!("swap {} recreated at new location ({note})", step.part));
            Ok(None)
        }
        Err(e) => {
            let hinted = step.dst_first.saturating_mul(sector_size);
            Ok(Some(Pending::new(
                step.part,
                PendingKind::Swap,
                e.to_string(),
                // hint 指向表项已提交的新位置：只有读旧签名区取身份才用旧起点
                crate::fsops::rescue_hint(
                    Some(ContentKind::Swap),
                    &crate::dev::part_dev_hint(src, step.part, hinted),
                    None,
                ),
            )))
        }
    }
}
