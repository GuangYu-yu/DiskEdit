//! undo：回滚恢复记录里记下的原始字节（分区表等直接写入）。

use crate::cli::UndoRequest;
use crate::dev::{FileSource, WriteSource};
use crate::support::*;

pub(crate) const DETAILS: &str = r#"  Roll back this target's recovery record: put back the bytes the recorded
  operation overwrote — partition-table entries and the in-place filesystem
  metadata it changed (NTFS backup sector pointer). Relocated data cannot be
  rolled back, and writes made by external FS tools are not undone.

  A legacy undo journal is not a recovery record; this build does not replay,
  migrate, or delete it.

  Example: diskedit undo /dev/sdb --yes"#;

/// 补充凭据的核对。记录侧没有可比的值（不可得 / 本次生成）时不参与比较——它不提供额外
/// 判据，也绝不表述成"相符"；记录侧有 GUID 而当前读不出时是"读不出"而非"不符"（4），
/// 不能落成可重试的账
fn verify_disk_identity(
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

/// 盘上字节是否与本条记录相容：等于写后内容（未回滚）、等于原文（已回滚，重放幂等），
/// 或是一条**前缀已落地**的写入——`write_all` 在中途失败或掉电时，落盘的只能是写后
/// 内容的一段前缀加上其后的原文。第三种形态既不是 after 也不是 pre-image，只按前两者
/// 判定会把本工具自己没写完的区间报成"外来改动"，让整卷回滚失去出路
fn matches_recorded_write(now: &[u8], after: &[u8], pre: &[u8]) -> bool {
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
fn verify_copy_source(src: &FileSource, record: &crate::recovery::Record, path: &std::path::Path) {
    if record.op != crate::recovery::OpKind::Copy || record.restore.is_empty() {
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
    let now = range_sha256(src, offset, len).unwrap_or_else(|e| {
        bail_fail(RunOutcome::no_mutation(format!(
            "cannot read source partition #{} of the recovery record ({e}) — nothing was written",
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

/// 回滚单文件恢复记录（`<image>.diskedit`）。返回 `None` 表示落点没有记录。
///
/// 载荷是"被覆盖扇区的原始字节 + 写入后字节 + LBA 区间 + 扇区大小"，回放只把字节逆序
/// 写回原位，不重新解析表语义。**写第一个字节前先核对盘面**：每条区间的当前字节必须
/// 仍是记录写后的样子（或已回到原文），否则拒绝，不做覆盖。
///
/// 载荷尾部撕裂时，逐条 CRC 仍能验证的完整前缀照常回放并退 5；记录改名归档保留，
/// 后半截没有任何可验证的边界，不猜。
fn undo_record(src: &mut WriteSource) -> Option<u8> {
    let path = src.identity.record_path()?.to_path_buf();
    // 落点缺席是常态：判据用平台无关的探测——没有目录相对原语的平台上
    // [`read_companion`] 一律报"读不出来"，会把缺席也算成故障
    if matches!(crate::recovery::probe_companion(&path), crate::recovery::CompanionProbe::Absent) {
        return None;
    }
    let current = match crate::platform::probe::probe_of(&*src) {
        Ok(probe) => crate::recovery::SceneIdentity::of_source(&probe, &*src),
        Err(e) => bail_fail(RunOutcome::no_mutation(format!(
            "cannot establish the target identity for the recovery record ({e:?}) — nothing was read or written"
        ))),
    };
    let evidence = match crate::recovery::read_companion(&path, &current) {
        Ok(evidence) => evidence,
        Err(e) => bail_fail(RunOutcome::no_mutation(format!(
            "the recovery record cannot be read ({e}) — nothing was read or written; release it with `diskedit abandon`"
        ))),
    };
    let record = match evidence {
        crate::recovery::Evidence::Absent => return None,
        crate::recovery::Evidence::Recognized(record) => record,
        crate::recovery::Evidence::ForeignTarget { recorded, current } => {
            let why = recorded
                .mismatch_reason(&current)
                .unwrap_or_else(|| "the recorded identity differs from the target".to_string());
            bail_fail(RunOutcome::refused(format!(
                "the recovery record at {} was written for a different target ({why}) — it is not replayed here; \
                 release it with `diskedit abandon`",
                path.display()
            )))
        }
        other => {
            // 整份载荷验证不过：完整前缀仍可能逐条验证并回放
            #[cfg(target_os = "linux")]
            return Some(undo_record_prefix(src, &path, &current, other));
            #[cfg(not(target_os = "linux"))]
            bail_fail(RunOutcome::refused(format!(
                "the recovery record at {} is not replayable ({other:?}) — it is left in place; \
                 release it with `diskedit abandon`",
                path.display()
            )))
        }
    };
    if record.commit == crate::recovery::CommitState::Committed {
        // 提交后未清理：目标字节已定型，本次运行只释放记录
        let outcome = match crate::recovery::settle_companion(&path, &record, false) {
            Ok(()) => table_write_done(
                src,
                "the recorded operation had already committed — its recovery record was released and nothing was \
                 rolled back (verify with: diskedit info)",
            ),
            Err(e) => {
                let msg = format!(
                    "the recorded operation had already committed, but its recovery record could not be released ({e}); \
                     the record is left in place"
                );
                // 回读不符是「要删的不是这份记录」的拒绝，不是收尾故障：本次未写目标字节，落 3
                if matches!(e, crate::recovery::RecordJournalError::ReadbackMismatch(_)) {
                    RunOutcome::refused(format!("{msg} — release it with `diskedit abandon`"))
                } else {
                    RunOutcome::partial(msg)
                }
            }
        };
        return Some(outcome.exit_code());
    }
    // `move` / `copy` 的已搬数据不回滚，但 preimage 只覆盖表区、仍可回写。
    // 这两条命令的数据写入必然越过不可逆点，若按 `restore_blocked` 一律拒绝，表区就再也
    // 没有回滚路径——那不是保守，是把契约规定的可回滚部分一并丢掉
    let table_only_replay = matches!(record.op, crate::recovery::OpKind::Move | crate::recovery::OpKind::Copy);
    if record.restore_blocked && !table_only_replay {
        bail_fail(RunOutcome::refused(format!(
            "the recovery record at {} marks a non-reversible mutation, so the recorded bytes are not written back; \
             release the transaction with `diskedit abandon`",
            path.display()
        )));
    }
    validate_record_ranges(src, &record.restore, &path);
    if let Err(f) = verify_disk_identity(record.disk_guid, src, "the recovery record") {
        bail_fail(f);
    }
    verify_copy_source(&*src, &record, &path);
    let complete = match verify_record_plan(&*src, &record.restore) {
        Ok(complete) => complete,
        Err(f) => bail_fail(f),
    };
    let n = record.restore.len();
    if complete {
        // 盘面已全部回到原文：回放没有可写的字节，直接收尾
        let mut outcome = crate::outcome::RunOutcome::applied_with(Vec::new());
        if let Err(e) = crate::recovery::settle_companion(&path, &record, true) {
            if matches!(e, crate::recovery::RecordJournalError::ReadbackMismatch(_)) {
                bail_fail(RunOutcome::refused(format!(
                    "the recovery record read back before deletion is not the one verified ({e}); \
                     the record is left in place — release it with `diskedit abandon`"
                )));
            }
            outcome = outcome.degrade_partial(format!(
                "the recovery record could not be released ({e}); release it with `diskedit abandon`"
            ));
        }
        let outcome = crate::support::settle_layout(outcome, src);
        if outcome.is_complete() {
            println!(
                "the target already held the recorded pre-images — nothing was written back (verify with: diskedit info)"
            );
        }
        return Some(outcome.exit_code());
    }
    replay_record(src, &record.restore, &path);
    let mut outcome = crate::outcome::RunOutcome::applied_with(Vec::new());
    if let Err(e) = crate::recovery::settle_companion(&path, &record, true) {
        // 码值按本次是否已写目标字节分流：写过目标字节只能报"完成但有待办"，
        // 没有可回放区间时清理侧拒绝仍是拒绝
        if crate::outcome::target_may_have_been_written() {
            outcome = outcome.degrade_partial(format!(
                "the rollback is complete but the recovery record could not be released ({e}); \
                 the record is left in place — release it with `diskedit abandon`"
            ));
        } else {
            bail_fail(RunOutcome::refused(format!(
                "the recovery record could not be released ({e}); the record is left in place — \
                 release it with `diskedit abandon`"
            )));
        }
    }
    let taking = if n == 0 {
        "the recovery record held no written ranges — nothing was written back (verify with: diskedit info)"
            .to_string()
    } else {
        format!("undid {n} recorded write range(s) (verify with: diskedit info)")
    };
    let outcome = crate::support::settle_layout(outcome, src);
    if outcome.is_complete() {
        println!("{taking}");
    }
    Some(outcome.exit_code())
}

/// 整份载荷验证不过时的前缀回放：只回滚逐条 CRC 通过的完整前缀，归档记录并退 5。
/// 前缀里任何一条对不上盘面都拒绝（3），一个字节也不写。
#[cfg(target_os = "linux")]
fn undo_record_prefix(
    src: &mut WriteSource,
    path: &std::path::Path,
    current: &crate::recovery::SceneIdentity,
    evidence: crate::recovery::Evidence,
) -> u8 {
    let prefix = match crate::recovery::read_companion_prefix(path) {
        Ok(Some(prefix)) => prefix,
        Ok(None) => bail_fail(RunOutcome::refused(format!(
            "the recovery record at {} is not replayable ({evidence:?}) — it is left in place; \
             release it with `diskedit abandon`",
            path.display()
        ))),
        Err(e) => bail_fail(RunOutcome::no_mutation(format!(
            "the recovery record cannot be read ({e}) — nothing was read or written; release it with `diskedit abandon`"
        ))),
    };
    if let Some(why) = prefix.record.target.mismatch_reason(current) {
        bail_fail(RunOutcome::refused(format!(
            "the recovery record at {} was written for a different target ({why}) — it is not replayed here; \
             release it with `diskedit abandon`",
            path.display()
        )));
    }
    if prefix.record.commit == crate::recovery::CommitState::Committed {
        let mut outcome = crate::outcome::RunOutcome::applied_with(Vec::new());
        match archive_torn(path) {
            Ok(to) => outcome = outcome.degrade_partial(format!(
                "the record's payload is incomplete and belonged to an already committed operation — nothing was \
                 rolled back; the torn record was archived as {}",
                to.display()
            )),
            Err(why) => {
                outcome = outcome.degrade_partial(format!(
                    "the record's payload is incomplete and belonged to an already committed operation — nothing was \
                     rolled back; {why}"
                ))
            }
        }
        let outcome = crate::support::settle_layout(outcome, src);
        return outcome.exit_code();
    }
    if prefix.record.restore_blocked {
        bail_fail(RunOutcome::refused(format!(
            "the recovery record at {} marks a non-reversible mutation, so the recorded bytes are not written back; \
             release the transaction with `diskedit abandon`",
            path.display()
        )));
    }
    // 撕裂载荷的末段无法验证，`copy` 的源区间就读不回来：没有源复核就不回放它的表区
    if prefix.record.op == crate::recovery::OpKind::Copy {
        bail_fail(RunOutcome::refused(format!(
            "the recovery record at {} is torn, so the source range its `copy` recorded cannot be read back — its \
             bytes are not written back; release it with `diskedit abandon`",
            path.display()
        )));
    }
    validate_record_ranges(src, &prefix.record.restore, path);
    if let Err(f) = verify_disk_identity(prefix.record.disk_guid, src, "the recovery record") {
        bail_fail(f);
    }
    if let Err(f) = verify_record_plan(&*src, &prefix.record.restore) {
        bail_fail(f);
    }
    replay_record(src, &prefix.record.restore, path);
    let mut outcome = crate::outcome::RunOutcome::applied_with(Vec::new());
    let kept = prefix.kept;
    let total = prefix.total;
    match archive_torn(path) {
        Ok(to) => {
            outcome = outcome.degrade_partial(format!(
                "the record's payload is incomplete: replayed the {kept} verified range(s) before the tear \
                 (of {total} recorded) and archived the torn record as {} — the ranges past the tear were not \
                 rolled back",
                to.display()
            ))
        }
        Err(why) => {
            outcome = outcome.degrade_partial(format!(
                "the record's payload is incomplete: replayed the {kept} verified range(s) before the tear \
                 (of {total} recorded), but {why}"
            ))
        }
    }
    let outcome = crate::support::settle_layout(outcome, src);
    outcome.exit_code()
}

/// 区间先整体核对：越界或形状不成立意味着这份记录描述的不是这张盘，
/// 拒绝发生在第一个字节写回之前
fn validate_record_ranges(
    src: &WriteSource,
    restore: &[crate::recovery::RestoreEntry],
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
fn verify_record_plan(
    src: &FileSource,
    restore: &[crate::recovery::RestoreEntry],
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

/// 逆序写回原文，然后逐条读回、sync。任一步失败都保留记录并退 5
fn replay_record(
    src: &mut WriteSource,
    restore: &[crate::recovery::RestoreEntry],
    path: &std::path::Path,
) {
    // 逆序：同一区间被后续写入覆盖时，先回放后写的那段，链首原文才落得回去
    for entry in restore.iter().rev() {
        let offset = entry.byte_offset().expect("validated before replay");
        if let Err(e) = src.write_at(offset, &entry.original) {
            bail_fail(RunOutcome::partial(format!(
                "undo write failed at offset {offset}: {e} — the recovery record at {} is kept; retry `diskedit undo`",
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

/// 旧 journal 不是恢复记录：它早于记录格式，本构建不回放、不迁移、不删除。
/// 落点上只要存在条目（含符号链接与 dangling 链接）就按未识别证据拒绝（3）。
fn refuse_legacy_journal(src: &FileSource) {
    let path = src.identity.journal_path();
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => bail_fail(RunOutcome::no_mutation(format!(
            "cannot determine whether a legacy undo journal exists at {}: {e} — nothing was written",
            path.display()
        ))),
        Ok(_) => bail_fail(RunOutcome::refused(format!(
            "a legacy undo journal exists at {}; it predates the recovery record format, so this build cannot \
             replay, migrate, or delete it — a legacy journal is not undoable. It is left in place for manual handling",
            path.display()
        ))),
    }
}

/// `undo` 只认目标身份派生的那一份恢复记录。记录缺席是常态：请求与现状不符（3）。
pub(crate) fn cmd_undo(a: &UndoRequest) -> u8 {
    // undo 回滚的是整盘记录：`:N` 指定了也会被静默忽略（与 info 同判据）
    if let Some(n) = a.part {
        bail_fail(RunOutcome::refused(format!("`undo` rolls back the whole target's recovery record — drop :{n}")));
    }
    // 回放类命令的载体必须是整盘：记录记的是整盘偏移，落到分区节点上会把
    // 别处的字节写进该分区
    refuse_partition_node_target("undo", &a.target);
    if !a.yes {
        bail_fail(RunOutcome::refused("`undo` overwrites current bytes from the recovery record; pass --yes to confirm"));
    } else {
        let mut src = open_target_owned(a).unwrap_or_else(|f| bail_fail(f));
        // 回放写回的是整盘表字节：目标仍在被使用时回放，会让内核分区视图与盘面
        // 从"分叉"变成"按历史布局分叉"。闸门在任何写回与记录处理之前
        if let Err(f) = crate::fsops::ensure_table_write_idle(&src) {
            bail_fail(f);
        }
        if let Some(code) = undo_record(&mut src) {
            return code;
        }
        refuse_legacy_journal(&src);
        bail_fail(RunOutcome::refused(format!("no recovery record on {} — nothing to undo", a.target)))
    }
}

/// 撕裂尾记录的归档后缀。首选名被占用时顺延为 `.torn.N`，绝不覆盖既有归档——
/// 那一份同样是证据（与 `abandon` 的 `.abandoned.N` 同一形态）
#[cfg(target_os = "linux")]
const TORN_SUFFIX: &str = crate::dev::QUARANTINE_TORN;

/// 把撕裂尾记录移出落点：记录文件进记录目录的隐藏名 `.torn[.N]`。建副本与删原件的
/// 顺序、同 inode 收敛与"绝不覆盖"判据见 [`crate::dev::quarantine_move`]
#[cfg(target_os = "linux")]
fn archive_torn(from: &std::path::Path) -> Result<std::path::PathBuf, String> {
    crate::dev::quarantine_move(from, TORN_SUFFIX)
        .map_err(|e| format!("cannot archive the recovery record at {}: {e}", from.display()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::let_underscore_must_use)] // 清理临时文件有意忽略失败
    use crate::dev::FileSource;
    use std::path::PathBuf;

    fn open_src(img: &std::path::Path) -> FileSource {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
        let size = std::fs::metadata(img).unwrap().len();
        FileSource {
            identity: crate::dev::TargetIdentity::resolve_image(img),
            file,
            path: img.to_path_buf(),
            sector_size: 512,
            size,
            is_block: false,
            record_journal: None,
            fingerprint: Default::default(),
            loop_mapping: None,
            owner: None,
        }
    }

    fn tmp_image(tag: &str) -> (crate::testdir::TestDir, PathBuf) {
        let dir = crate::testdir::test_dir(tag);
        let p = dir.join("undo.img");
        (dir, p)
    }

    /// 写一个只满足头部校验的 GPT 主头：凭据读取只依赖头（GUID 由头 CRC 覆盖）
    fn write_gpt_primary(img: &std::path::Path, guid: [u8; 16]) {
        const SS: usize = 512;
        let mut data = std::fs::read(img).unwrap();
        let last = (data.len() / SS) as u64 - 1;
        let mut hdr = vec![0u8; SS];
        hdr[0..8].copy_from_slice(b"EFI PART");
        hdr[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        hdr[12..16].copy_from_slice(&92u32.to_le_bytes());
        hdr[24..32].copy_from_slice(&1u64.to_le_bytes());
        hdr[32..40].copy_from_slice(&last.to_le_bytes());
        hdr[56..72].copy_from_slice(&guid);
        let crc = crate::table::crc32(&hdr[..92]);
        hdr[16..20].copy_from_slice(&crc.to_le_bytes());
        data[SS..SS * 2].copy_from_slice(&hdr);
        std::fs::write(img, data).unwrap();
    }

    #[test]
    fn disk_guid_is_read_from_the_gpt_header() {
        let (_dir, img) = tmp_image("guid");
        std::fs::write(&img, vec![0u8; 4096]).unwrap();
        write_gpt_primary(&img, [0x5A; 16]);
        let src = open_src(&img);
        assert_eq!(crate::table::disk_guid_of(&src).unwrap(), Some([0x5A; 16]));
    }

    /// "读不出"与"没有 GPT"必须是两回事：签名在而头校验不过 → Err（Damaged 出口），
    /// 任何头签名都没有 → Ok(None)（不可得）
    #[test]
    fn a_damaged_gpt_header_is_unreadable_not_absent() {
        let (_dir, img) = tmp_image("guid_bad");
        std::fs::write(&img, vec![0u8; 4096]).unwrap();
        assert_eq!(
            crate::table::disk_guid_of(&open_src(&img)).unwrap(),
            None,
            "a target without any GPT header must be Unavailable"
        );
        write_gpt_primary(&img, [0x5A; 16]);
        // 破坏头 CRC（签名仍在）：凭据读不出
        let mut data = std::fs::read(&img).unwrap();
        data[512 + 16] ^= 0x01;
        std::fs::write(&img, data).unwrap();
        assert!(
            crate::table::disk_guid_of(&open_src(&img)).is_err(),
            "a present-but-broken GPT header is unreadable, not absent"
        );
    }

    /// 相容形态有三种：写后内容、原文（上一次回滚的进度），以及一次没写完的写留下的
    /// "after 前缀 + pre 后缀"。外来改动不属于任何一种，必须继续拒绝
    #[test]
    fn recorded_write_accepts_a_torn_prefix_but_not_a_foreign_mix() {
        let after = [0xAAu8; 8];
        let pre = [0x11u8; 8];
        assert!(super::matches_recorded_write(&after, &after, &pre));
        assert!(super::matches_recorded_write(&pre, &after, &pre), "a replayed region is idempotent");
        let mut torn = pre;
        torn[..5].copy_from_slice(&after[..5]);
        assert!(
            super::matches_recorded_write(&torn, &after, &pre),
            "the landed prefix of this tool's own write must stay replayable"
        );
        assert!(!super::matches_recorded_write(&[0x22u8; 8], &after, &pre), "a foreign edit is not a torn write");
    }
}
