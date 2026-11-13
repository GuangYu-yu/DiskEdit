//! 回滚的签发与执行。
//!
//! 准入核对（记录资格、身份、区间形状、源复核、盘面比对）全部通过后才产出
//! [`ProvisionalRecord`] 与 [`RollbackPermit`]；[`execute_rollback`] 按值消费两者，
//! 把记录里的原字节逆序写回并释放记录。两个凭据都不可克隆：构造字段私有，
//! crate 内除签发点外拿不到，回放的每一步写入因此都处在同一套核对之下。

use super::{OpKind, Record, RestoreEntry, Stage, settle_companion};
use crate::dev::{FileSource, WriteSource};
use crate::outcome::RunOutcome;
use crate::support::bail_fail;

/// 逐条核对失败后共用的出路：两种成因（盘上字节被外部改动、目标被缩短到记录之外）下
/// "重跑 undo"都要等目标回到那条写入时的状态之后才可能成功。确实不再需要回滚时，
/// `abandon` 释放现场——它不写目标一个字节，放弃的是"还能回滚"这个能力
const ROLLBACK_STUCK_REMEDY: &str =
    "restore the target to the state it was in, then re-run `undo`; if the rollback is no longer \
     wanted, release it with `diskedit abandon`";

/// 外部改动之后的另一半事实：记录只记盘上字节，内核分区视图是否跟上是另一回事，
/// 而后续写命令与 FS 工具都按内核视图动手
const FOREIGN_EDIT_HINT: &str =
    "if another tool rewrote the table, run partprobe on the disk so the kernel partition view \
     matches the disk before retrying";

/// 撕裂载荷归档的后缀：归档保留记录全文，留给人工核验
pub(crate) const TORN_SUFFIX: &str = crate::dev::QUARANTINE_TORN;

/// 准入通过的进行中记录。字段私有，只能由 [`RecoveryEngine::admit_rollback`] 产出
pub(crate) struct ProvisionalRecord {
    record: Record,
    path: std::path::PathBuf,
}

/// 回滚写入许可。不可克隆、按值消费，只由 [`RecoveryEngine::admit_rollback`] 在
/// 全部核对通过后签发
pub(crate) struct RollbackPermit {
    _seal: Seal,
}

/// 签发点的封印：字段私有，其它模块构造不了带它的值
struct Seal;

/// `restore_blocked` 的豁免口径：`move` / `copy` 的数据搬移越过不可逆点后，表区
/// preimage 仍在记录里——回放表区、不碰数据区。
///
/// `undo` 的准入、`inspect` 的下一步提示、`abandon` 的放弃计数与"目标忙"的出路文案
/// 共用这一份判据：各处各写一遍匹配式，就会出现"提示说能撤、`undo` 说不能"的漂移
pub(crate) fn table_only_replay(op: OpKind) -> bool {
    matches!(op, OpKind::Move | OpKind::Copy)
}

/// 准入结论。`Proceed` 携带写入许可；`AlreadyReverted` 表示没有需要写回的字节
/// （记录里的区间都已回到原文，或记录里没有可回写区间且阶段仍停在初始态），把记录与落点
/// 交还调用方做零写入收尾
pub(crate) enum Admission {
    Proceed(ProvisionalRecord, RollbackPermit),
    AlreadyReverted(Record, std::path::PathBuf),
}

impl crate::recovery::RecoveryEngine {
    /// 回滚准入。逐项核对后签发：资格、身份、区间形状、copy 源复核、盘面比对；
    /// 盘面已全部回到原文时不签发许可，按 `AlreadyReverted` 走零写入收尾。
    /// 返回即已报告的拒绝（3/4）经 [`bail_fail`] 直接退出
    pub(crate) fn admit_rollback(
        src: &WriteSource,
        record: Record,
        path: &std::path::Path,
    ) -> Admission {
        // `move` / `copy` 的已搬数据不回滚，但 preimage 只覆盖表区、仍可回写。
        // 这两条命令的数据写入必然越过不可逆点，若按 `restore_blocked` 一律拒绝，表区
        // 就再也没有回滚路径——那不是保守，是把可回滚的部分一并丢掉
        if record.restore_blocked && !table_only_replay(record.op) {
            bail_fail(RunOutcome::refused(format!(
                "the recovery record at {} marks a non-reversible mutation, so the recorded bytes are not written back; \
                 release the transaction with `diskedit abandon`",
                path.display()
            )));
        }
        validate_record_capacity(src, &record, path);
        validate_record_ranges(src, &record.restore, path);
        if let Err(f) = verify_disk_identity(record.disk_guid, src, "the recovery record") {
            bail_fail(f);
        }
        verify_copy_source(src, &record, path);
        let complete = match verify_record_plan(src, &record.restore) {
            Ok(complete) => complete,
            Err(f) => bail_fail(f),
        };
        if complete {
            // `restore` 只覆盖表区：它为空只说明表区没记过原文，推不出作业没开始。
            // 记录里还留着什么按派生阶段判：阶段越过 `Planned` 时剩下的就是进度
            // （`move` 数据区的唯一副本），签发 `AlreadyReverted` 会把它当终态删掉
            let stage = record.derive_stage().unwrap_or_else(|f| bail_fail(f));
            // 阶段由记录字段派生：`Planned` 就是"还没动手"，不再另判一次进度字段
            if record.restore.is_empty() && stage != Stage::Planned {
                bail_fail(RunOutcome::refused(format!(
                    "the recovery record at {} holds no table pre-image, and an empty pre-image set only says \
                     the table was not recorded, not that the operation is already rolled back; {}",
                    path.display(),
                    unfinished_stage_remedy(&record, stage)
                )));
            }
            return Admission::AlreadyReverted(record, path.to_path_buf());
        }
        Admission::Proceed(ProvisionalRecord { record, path: path.to_path_buf() }, RollbackPermit { _seal: Seal })
    }
}

/// 记录里还留着进度时，按派生阶段指向该走的路：续跑、表已提交后的收尾，或释放现场。
/// 游标到终点不等于作业走完——表已提交与收尾未做是两份不同的事实
fn unfinished_stage_remedy(record: &Record, stage: Stage) -> String {
    let command = record.op.command_name();
    match stage {
        Stage::Relocating { entry, .. } => format!(
            "the recorded relocation stopped at entry {entry}; re-run `diskedit {command}` with the same \
             request to continue it, or release the record with `diskedit abandon` only if it is stale"
        ),
        Stage::Relocated => format!(
            "the data for the current relocation entry is already copied and its cursor was not advanced, so \
             the recorded progress cannot be discarded; re-run `diskedit {command}` with the same request to \
             continue it, or release the record with `diskedit abandon` only if it is stale"
        ),
        Stage::TableWritten => format!(
            "the partition table write is already on the target and the steps after it are still pending; \
             re-run `diskedit {command}` with the same request to finish them (`diskedit inspect` shows what \
             is left), or release the record with `diskedit abandon` only if it is stale"
        ),
        Stage::Finishing => format!(
            "the partition step is done and the finishing steps are still pending; re-run `diskedit {command}` \
             with the same request to finish them, or release the record with `diskedit abandon` only if it is \
             stale"
        ),
        // 字段不足以判定阶段：不代做动作，只指路。有续跑路径的用原命令完成；
        // `copy` 明确不续跑，只能释放现场后整条重跑
        _ if record.op.resumable() => format!(
            "the record's fields do not decide how far the operation got; inspect it with `diskedit inspect`, \
             then re-run `diskedit {command}` with the same request to finish it, or release the record with \
             `diskedit abandon` only if it is stale"
        ),
        _ => format!(
            "`{command}` has no resume path and `undo` cannot remove the bytes it already wrote; release the \
             record with `diskedit abandon`, then run `diskedit {command}` again"
        ),
    }
}

/// 回滚的唯一写入入口：逆序写回原文、逐条读回、落盘，然后释放记录。
/// 码值按本次是否已写目标字节分流——写过目标字节的释放失败报"完成但有待办"，
/// 没有可回放区间时的释放失败仍是拒绝
pub(crate) fn execute_rollback(
    src: &mut WriteSource,
    provisional: ProvisionalRecord,
    permit: RollbackPermit,
) -> u8 {
    let _ = permit;
    let path = provisional.path;
    let record = provisional.record;
    let n = record.restore.len();
    replay_record(src, &record.restore, &path);
    let outcome = match settle_companion(&path, &record, true) {
        Ok(()) => RunOutcome::applied_with(Vec::new()),
        // 目录项已移除、持久化未确认：只陈述这一事实，不承诺记录还在
        Err(e) if !e.record_still_on_disk() => {
            RunOutcome::applied_with(Vec::new()).degrade_partial(format!("{e}"))
        }
        Err(e) => {
            let cause = format!(
                "the recovery record could not be released ({e}); the record is left in place — release it with `diskedit abandon`"
            );
            // 判据：记录仍在盘上而释放失败。已越过不可逆写入点的运行盘面可能已改，
            // 降级为 5 并保留记录；未越过的仍是拒绝
            let unwritten = cause.clone();
            crate::outcome::attribute(
                || RunOutcome::applied_with(Vec::new()).degrade_partial(cause),
                || bail_fail(RunOutcome::refused(unwritten)),
            )
        }
    };
    let taking = if n == 0 {
        "the recovery record held no written ranges — nothing was written back (verify with: diskedit info)"
            .to_string()
    } else {
        format!("undid {n} recorded write range(s) (verify with: diskedit info)")
    };
    let record = src.record_journal().map(|j| j.record());
    let outcome = crate::support::settle_layout(outcome, src, record);
    if outcome.is_complete() {
        println!("{taking}");
    }
    outcome.exit_code()
}

/// 补充凭据的核对。记录侧没有可比的值（不可得 / 本次生成）时不参与比较——它不提供额外
/// 判据，也绝不表述成"相符"；记录侧有 GUID 而当前读不出时是"读不出"而非"不符"（4），
/// 不能落成可重试的账
pub(crate) fn verify_disk_identity(
    recorded: crate::dev::DiskGuidState,
    src: &FileSource,
    left: &str,
) -> Result<(), RunOutcome> {
    if !matches!(recorded, crate::dev::DiskGuidState::Present(_)) {
        return Ok(());
    }
    let current = match crate::table::disk_guid_of(src) {
        Ok(Some(g)) => crate::dev::DiskGuidState::Present(g),
        Ok(None) => crate::dev::DiskGuidState::Unavailable,
        Err(e) => {
            return Err(RunOutcome::no_mutation(format!(
                "the target's GPT disk identity cannot be read ({e}) — {left}; {ROLLBACK_STUCK_REMEDY}"
            )))
        }
    };
    match crate::dev::disk_identity_mismatch(recorded, current) {
        Some(why) => Err(RunOutcome::refused(format!("{why} — {left}; {ROLLBACK_STUCK_REMEDY}"))),
        None => Ok(()),
    }
}

/// 盘上字节是否与本条记录相容：等于写后内容（未回滚）、等于原文（已回滚，重放幂等），
/// 或是一条**前缀已落地**的写入——`write_all` 在中途失败或掉电时，落盘的只能是写后
/// 内容的一段前缀加上其后的原文。第三种形态既不是 after 也不是 pre-image，只按前两者
/// 判定会把本工具自己没写完的区间报成"外来改动"，让整卷回滚失去出路
pub(crate) fn matches_recorded_write(now: &[u8], after: &[u8], pre: &[u8]) -> bool {
    if now == after || now == pre {
        return true;
    }
    if now.len() != after.len() || now.len() != pre.len() {
        return false;
    }
    // 存在 k 使 now[..k] == after[..k] 且 now[k..] == pre[k..]：前缀相符长度 p 与
    // 后缀相符长度 s 能覆盖整个区间（p + s ≥ len）即存在这样的 k
    let p = now.iter().zip(after).take_while(|(a, b)| a == b).count();
    let s = now.iter().zip(pre).rev().take_while(|(a, b)| a == b).count();
    p + s >= now.len()
}

/// `copy` 记录回滚前的源复核。源与副本同属一个目标，源身份由记录目标的身份承担——
/// 读记录时已按它分类，`ForeignTarget` 直接拒绝；这里重读记录里的源区间并与摘要比对。
/// 缺段、区间算不出或读不出都不写回一个字节：`copy` 的数据在表区之外，源已变时回滚表区
/// 救不回那次复制。没有可回写的区间时不读源——没有回放，也就没有要护住的东西
fn verify_copy_source(src: &FileSource, record: &Record, path: &std::path::Path) {
    if record.op != OpKind::Copy || record.restore.is_empty() {
        return;
    }
    let Some(source) = record.source else {
        bail_fail(RunOutcome::refused(format!(
            "the recovery record at {} carries no source range, so the bytes its `copy` wrote cannot be checked \
             against the source partition — they are not written back; release it with `diskedit abandon`",
            path.display()
        )));
    };
    let (Some(offset), Some(len)) = (source.byte_offset(), source.byte_len()) else {
        bail_fail(RunOutcome::refused(format!(
            "the source range in the recovery record at {} overflows the byte space — its bytes are not written \
             back; release it with `diskedit abandon`",
            path.display()
        )));
    };
    if offset.checked_add(len).is_none_or(|end| end > src.size) {
        bail_fail(RunOutcome::refused(format!(
            "source partition #{} of the recovery record at {} now lies outside the target — its bytes are not \
             written back; {ROLLBACK_STUCK_REMEDY}",
            source.partition,
            path.display()
        )));
    }
    let now = crate::support::range_sha256(src, offset, len).unwrap_or_else(|e| {
        bail_fail(RunOutcome::no_mutation(format!(
            "cannot read source partition #{} of the recovery record ({e})",
            source.partition
        )))
    });
    if now != source.digest {
        bail_fail(RunOutcome::refused(format!(
            "source partition #{} no longer holds the bytes recorded when the copy ran (digest differs) — the \
             recorded bytes are not written back; {ROLLBACK_STUCK_REMEDY}",
            source.partition
        )));
    }
}

/// 记录里的目标容量与当前容量不符即拒绝：回放按记录里的 LBA 与字节数写回，
/// 容量变了说明这份记录描述的不是当前这张盘。`None` 是没有记下容量，不参与判定
pub(crate) fn validate_record_capacity(src: &WriteSource, record: &Record, path: &std::path::Path) {
    let Some(recorded) = record.target.size() else {
        return;
    };
    if recorded != src.size {
        bail_fail(RunOutcome::refused(format!(
            "the target size changed since the recovery record was written (recorded {recorded} byte(s), \
             current {} byte(s)) — the recorded bytes are not written back; release the record at {} with \
             `diskedit abandon`",
            src.size,
            path.display()
        )));
    }
}

/// 区间先整体核对：越界或形状不成立意味着这份记录描述的不是这张盘，
/// 拒绝发生在第一个字节写回之前
pub(crate) fn validate_record_ranges(
    src: &WriteSource,
    restore: &[RestoreEntry],
    path: &std::path::Path,
) {
    for entry in restore {
        let Some(offset) = entry.byte_offset() else {
            bail_fail(RunOutcome::refused(format!(
                "the recovery record at {} has an unrepresentable range (LBA {} × {} byte sector(s)); \
                 it is left in place — release it with `diskedit abandon`",
                path.display(),
                entry.first_lba,
                entry.sector_size
            )));
        };
        let end = offset.saturating_add(entry.original.len() as u64);
        if !entry.is_well_formed() || end > src.size {
            bail_fail(RunOutcome::refused(format!(
                "the recovery record at {} has a range {offset}..{end} that does not fit this target \
                 ({} byte(s)); it is left in place — release it with `diskedit abandon`",
                path.display(),
                src.size
            )));
        }
    }
}

/// 写盘前的盘面核对。逐条逆序，用虚拟盘面叠加已核对过的后写区间，判定每条当前字节是
/// 记录写后的样子、还是已经回到原文（上一轮回放的进度）。
/// 返回 `Ok(true)` 表示整份记录覆盖的区间都已是原文（没有需要写回的字节）。
/// 对不上盘面时返回拒绝（3），读不出时返回 4：两者都不写目标一个字节
pub(crate) fn verify_record_plan(
    src: &FileSource,
    restore: &[RestoreEntry],
) -> Result<bool, RunOutcome> {
    let mut patches: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut complete = true;
    for entry in restore.iter().rev() {
        let offset = entry.byte_offset().expect("ranges were validated before verification");
        let now = read_virtual(src, offset, entry.original.len(), &patches)?;
        if now != entry.original {
            if !matches_recorded_write(&now, &entry.after, &entry.original) {
                return Err(RunOutcome::refused(format!(
                    "the target no longer holds the recorded bytes at offset {offset} — the disk changed outside \
                     this tool after that write, or an interrupted write by this tool left a torn value; \
                     replaying would overwrite the foreign edit; {FOREIGN_EDIT_HINT}; {ROLLBACK_STUCK_REMEDY}"
                )));
            }
            complete = false;
        }
        patches.push((offset, entry.original.clone()));
    }
    Ok(complete)
}

/// 虚拟盘面读：先读真盘，再按回放顺序叠加已核对区间的原文（后叠加的覆盖先叠加的）
fn read_virtual(
    src: &FileSource,
    offset: u64,
    len: usize,
    patches: &[(u64, Vec<u8>)],
) -> Result<Vec<u8>, RunOutcome> {
    let mut buf = vec![0u8; len];
    src.read_at(offset, &mut buf).map_err(|e| {
        RunOutcome::no_mutation(format!(
            "cannot verify the recorded write at offset {offset} against the target: {e} — refusing to replay \
             over an unverifiable region; {ROLLBACK_STUCK_REMEDY}"
        ))
    })?;
    let end = offset.saturating_add(len as u64);
    for (patch_off, bytes) in patches {
        let patch_end = patch_off.saturating_add(bytes.len() as u64);
        let start = (*patch_off).max(offset);
        let stop = patch_end.min(end);
        if start >= stop {
            continue;
        }
        let from = (start - offset) as usize;
        let to = (stop - offset) as usize;
        let at = (start - *patch_off) as usize;
        buf[from..to].copy_from_slice(&bytes[at..at + (to - from)]);
    }
    Ok(buf)
}

/// 回放凭据：携带一条记录条目，`WriteSource::write_recorded` 按它写回原文。
///
/// 字段私有，铸造点只有 [`replay_record`]，写记录原文的能力因此只落在回放路径上
pub(crate) struct RecordedPreImage<'a> {
    entry: &'a RestoreEntry,
}

impl<'a> RecordedPreImage<'a> {
    fn of(entry: &'a RestoreEntry) -> Self {
        Self { entry }
    }

    pub(crate) fn offset(&self) -> u64 {
        self.entry.byte_offset().expect("validated before replay")
    }

    pub(crate) fn bytes(&self) -> &'a [u8] {
        self.entry.original.as_slice()
    }
}

/// 逆序写回原文，然后逐条读回、sync。任一步失败都保留记录并退 5
pub(crate) fn replay_record(
    src: &mut WriteSource,
    restore: &[RestoreEntry],
    path: &std::path::Path,
) {
    // 逆序：同一区间被后续写入覆盖时，先回放后写的那段，链首原文才落得回去
    for entry in restore.iter().rev() {
        let pre_image = RecordedPreImage::of(entry);
        if let Err(e) = src.write_recorded(&pre_image) {
            bail_fail(RunOutcome::partial(format!(
                "undo write failed at offset {}: {e} — the recovery record at {} is kept; retry `diskedit undo`",
                pre_image.offset(),
                path.display()
            )));
        }
    }
    // 写后读回：缓存丢写与镜像层短写只有重读才可见。回放是唯一的回退路径，
    // 这一段丢了就没有第二处能发现
    for entry in restore {
        let offset = entry.byte_offset().expect("validated before replay");
        let mut got = vec![0u8; entry.original.len()];
        match src.read_at(offset, &mut got) {
            Ok(()) if got == entry.original => {}
            Ok(()) => bail_fail(RunOutcome::partial(format!(
                "undo wrote the pre-image at offset {offset} but the target holds different bytes; \
                 the recovery record at {} is kept — retry `diskedit undo`",
                path.display()
            ))),
            Err(e) => bail_fail(RunOutcome::partial(format!(
                "cannot read back the pre-image just written at offset {offset}: {e}; \
                 the recovery record at {} is kept — retry `diskedit undo`",
                path.display()
            ))),
        }
    }
    src.sync_all().unwrap_or_else(|e| {
        bail_fail(RunOutcome::partial(format!(
            "undo wrote the record's bytes back but sync failed: {e} — the rollback may not be durable, \
             the recovery record at {} is kept; retry `diskedit undo`",
            path.display()
        )))
    });
}

/// 记录改名到同目录的 `.torn`，目录 fsync 由改名函数承担
pub(crate) fn archive_torn(from: &std::path::Path) -> Result<std::path::PathBuf, String> {
    crate::dev::quarantine_move(from, TORN_SUFFIX)
        .map_err(|e| format!("cannot archive the recovery record at {}: {e}", from.display()))
}

#[cfg(test)]
mod tests {
    /// 相容形态有三种：写后内容、原文（上一次回滚的进度），以及一次没写完的写留下的
    /// "after 前缀 + pre 后缀"。外来改动不属于任何一种，必须继续拒绝
    #[test]
    fn recorded_write_accepts_a_torn_prefix_but_not_a_foreign_mix() {
        use super::matches_recorded_write;
        let after = [0xAAu8; 8];
        let pre = [0x11u8; 8];
        assert!(matches_recorded_write(&after, &after, &pre));
        assert!(matches_recorded_write(&pre, &after, &pre), "a replayed region is idempotent");
        let mut torn = pre;
        torn[..5].copy_from_slice(&after[..5]);
        assert!(
            matches_recorded_write(&torn, &after, &pre),
            "the landed prefix of this tool's own write must stay replayable"
        );
        assert!(!matches_recorded_write(&[0x22u8; 8], &after, &pre), "a foreign edit is not a torn write");
    }
}
