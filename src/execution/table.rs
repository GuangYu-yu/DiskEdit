//! 表步骤的执行：写前按 `regions` 读回比对 `expect_before`，写入计划期算好的字节，
//! 写后按 `regions` 读回比对 `expect_after`。
//!
//! 写入字节只取自步骤本身；本层不读盘推算新表内容。

use crate::outcome::RunOutcome;
use crate::table::{verify_regions_after, verify_regions_before, TableStep};
use crate::dev::WriteSource;

/// 执行一个已装配的表步骤。未装配（两端表戳为空）、写入段不在比对范围内或长度不符时
/// 这一步骤不写。
///
/// 这些判定在首次写入前的预扫描里已按内部错误拒绝过；执行期再次命中时本计划可能已经
/// 写过目标字节，无法断言未改动，故按「可能已改变」上报。
///
/// `wrote_target` 表示本计划在这一步之前是否已经写过目标字节，由调用方从执行进度状态读出。
pub(crate) fn execute(
    src: &mut WriteSource,
    step: &TableStep,
    wrote_target: bool,
) -> Result<(), RunOutcome> {
    let Some(expect_before) = step.expect_before else {
        return Err(RunOutcome::partial(
            "the table step carries no expected pre-write stamp — the plan was not assembled; this step wrote nothing, earlier steps may already have written the target",
        ));
    };
    let Some(expect_after) = step.expect_after else {
        return Err(RunOutcome::partial(
            "the table step carries no expected post-write stamp — the plan was not assembled; this step wrote nothing, earlier steps may already have written the target",
        ));
    };
    let aligned = step.writes.iter().all(|w| {
        w.bytes.len() as u64 == w.region.len && step.regions.contains(&w.region)
    });
    if !aligned {
        return Err(RunOutcome::partial(
            "the table step's writes do not match its regions — this step wrote nothing, earlier steps may already have written the target",
        ));
    }

    verify_regions_before(src, &step.regions, expect_before, wrote_target)?;
    for w in &step.writes {
        src.write_at(w.region.offset, &w.bytes).map_err(|e| {
            RunOutcome::partial(format!(
                "cannot write the table at byte {}: {e} — the on-disk state may have changed",
                w.region.offset
            ))
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
