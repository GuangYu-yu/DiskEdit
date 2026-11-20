//! 缩容的第一步：把文件系统缩到新尺寸，成功之后才允许改表项。
//!
//! GPT 的 `--end` / SIZE 与 MBR 的 SIZE 共用这一份实现：顺序（FS 先、表后）、
//! 记录里的缩容相位、回读拿不到落点时的待办，只有一处。

use super::*;

/// 缩 FS 之后的分流：改表继续，或停在“分区仍挂载”（此时改表等于在挂载中挪分区）
pub(crate) enum ShrinkFirst {
    Done,
    TargetStillMounted,
}

/// 缩容第一步。返回前已把 `fs_phase` 落进记录：
/// `Verified` 表示缩容已执行，`Unverified` 表示工具返回了但落点读不回来。
///
/// 缩容不落不可回滚标记：缩小之后再回滚表项只会得到“FS 小于分区”这一自洽状态，
/// 回滚仍然安全可用。判据是可逆性，不是“有没有调外部工具”。
// audit-allow: A-19
#[allow(clippy::too_many_arguments)]
pub(crate) fn shrink_fs_before_table(
    src: &mut WriteSource,
    part: u32,
    fstype: fsid::ContentKind,
    new_bytes: u64,
    resume: movepart::FsShrink,
    chunks: u64,
    post: &mut Vec<crate::outcome::Pending>,
    log: &mut dyn FnMut(&str),
) -> Result<ShrinkFirst, RunOutcome> {
    let mut shrink_done = false;
    if resume == movepart::FsShrink::Unverified {
        match crate::fsops::shrink_already_at(src, part, Some(fstype), new_bytes) {
            Some(true) => {
                save_resize_part_progress(src, chunks, movepart::FsShrink::Verified, 1)
                    .map_err(RunOutcome::from)?;
                log(&format!(
                    "filesystem already at {new_bytes} bytes (re-read) — shrink step skipped"
                ));
                movepart::fault_rs_after_fs_shrink();
                shrink_done = true;
            }
            Some(false) => {}
            None => {
                // 回读仍不可用：缩容是否到位无从判断，接着改表会切进未缩的元数据
                post.push(crate::outcome::Pending::new(
                    part,
                    crate::outcome::PendingKind::Other("fs shrink"),
                    "the filesystem size still cannot be read back, so the earlier shrink cannot be confirmed".to_string(),
                    "inspect the filesystem size by hand, then re-run the same command".to_string(),
                ));
                return Err(RunOutcome::partial_unverified(
                    "the earlier filesystem shrink cannot be confirmed",
                    std::mem::take(post),
                ));
            }
        }
    }
    if !shrink_done {
        let o = crate::fsops::shrink_fs(src, part, Some(fstype), new_bytes)
            .map_err(|e| RunOutcome::partial(e.to_string()))?;
        // 卸挂结论要在消费结果之前取出：`pending` 拿走 leak 之后无从再判
        let release_failed = o.release_failed();
        let unverified = match &o {
            crate::fsops::ShrinkRunOutcome::Unverified { why, .. } => Some(why.clone()),
            crate::fsops::ShrinkRunOutcome::Verified { .. } => None,
        };
        let line = o.ok_line(new_bytes);
        for p in o.pending(part) {
            post.push(p);
        }
        if let Some(why) = unverified {
            save_resize_part_progress(src, chunks, movepart::FsShrink::Unverified, 1)
                .map_err(RunOutcome::from)?;
            return Err(RunOutcome::partial_unverified(why, std::mem::take(post)));
        }
        save_resize_part_progress(src, chunks, movepart::FsShrink::Verified, 1)
            .map_err(RunOutcome::from)?;
        if let Some(l) = line {
            log(&l);
        }
        // 临时挂载点没卸掉 = 分区仍挂载中，改表等于在挂载中挪动分区
        if release_failed {
            return Ok(ShrinkFirst::TargetStillMounted);
        }
        movepart::fault_rs_after_fs_shrink();
    }
    Ok(ShrinkFirst::Done)
}

/// 推进记录里的阶段号 / 游标 / FS 相位
pub(crate) fn save_resize_part_progress(
    src: &mut WriteSource,
    chunks: u64,
    fs_shrunk: movepart::FsShrink,
    step: u32,
) -> std::io::Result<()> {
    match src.record_journal_mut() {
        Some(journal) => journal
            .record_progress(step, chunks, fs_shrunk as u8)
            .map_err(|e| e.into_io()),
        None => Ok(()),
    }
}
