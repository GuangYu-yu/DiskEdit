//! 表步骤的执行：逐段经受守卫写入口写入，每条写入段的写前比对紧邻该段写入，
//! 写后立刻回读比对；全部写完后仍按 `regions` 复核 `expect_after`。
//!
//! 写入字节只取自步骤本身；本层不读盘推算新表内容。

use crate::dev::WriteSource;
use crate::outcome::{RunOutcome, WriteStep};
use crate::table::{verify_regions_after, TableStep};

/// 执行一个已装配的表步骤。未装配（两端表戳为空）、写入段不在比对范围内或长度不符时
/// 这一步骤不写。
///
/// 这些判定在首次写入前的预扫描里已按内部错误拒绝过；执行期再次命中时本计划可能已经
/// 写过目标字节，无法断言未改动，故按「可能已改变」上报。
pub(crate) fn execute(src: &mut WriteSource, step: &TableStep) -> Result<(), RunOutcome> {
    if step.expect_before.is_none() {
        return Err(RunOutcome::partial(
            "the table step carries no expected pre-write stamp — the plan was not assembled; this step wrote nothing, earlier steps may already have written the target",
        ));
    }
    let Some(expect_after) = step.expect_after else {
        return Err(RunOutcome::partial(
            "the table step carries no expected post-write stamp — the plan was not assembled; this step wrote nothing, earlier steps may already have written the target",
        ));
    };
    let aligned = step.writes.iter().all(|w| {
        w.bytes.len() as u64 == w.region.len && step.regions.contains(&w.region)
    });
    if !aligned || step.before.len() != step.writes.len() {
        return Err(RunOutcome::partial(
            "the table step's writes do not match its regions — this step wrote nothing, earlier steps may already have written the target",
        ));
    }

    // 修复计划写的是同一批表区间，归属与常规表写入不同：诊断行据此区分"改表"与"修表"
    let write_step = match step.kind {
        crate::table::TableWriteKind::Repair => WriteStep::Repair,
        _ => WriteStep::Table,
    };
    // 写事实在 `write_raw` 调写入前就置位，早于本段的 `sync_all`：段写失败时，
    // 目标是否已被改过只由这一个判定点回答
    for (w, expect) in step.writes.iter().zip(&step.before) {
        // 写前比对与写入在同一入口内完成：快照之后被外部改动的区间不会静默覆盖
        src.write_converged(w.region.offset, &w.bytes, *expect, write_step)
            .map_err(|e| {
                // 写前重读失败、比对不等或写调用报错：写事实已置位时目标可能
                // 已改变，按 5 报；否则本次写没有成立，按 4 报
                let cause = format!(
                    "cannot write the table at byte {}: {e}",
                    w.region.offset
                );
                if crate::outcome::crossed_irreversible_point() {
                    RunOutcome::partial(cause)
                } else {
                    RunOutcome::no_mutation(cause)
                }
            })?;
        // 每段写完整段落盘，而不是全部写完再 flush 一次：一次 sync_all 只保证
        // "这些字节都已提交"，不保证四段按写序抵达介质。掉电可能留下"主头已写、
        // 主数组仍是旧内容"的组合；逐段落盘后写序即落盘序，任一中断点盘上都有
        // 一份 CRC 自洽的表（GPT 写序为备数组→备头→主数组→主头）
        src.sync_all().map_err(|e| {
            RunOutcome::partial(format!(
                "cannot flush the table at byte {}: {e} — the write may not have landed",
                w.region.offset
            ))
        })?;
    }
    verify_regions_after(src, &step.regions, expect_after)
}
