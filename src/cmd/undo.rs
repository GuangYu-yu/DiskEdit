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


fn undo_record(src: &mut WriteSource) -> Option<u8> {
    // 记录不按单一派生落点认：身份漂移或目录被改名后，派生路径找不到它。枚举把
    // 身份不符/无法比对的候选一并列出，但只认领身份严格相符的那些
    let records = crate::support::active_recovery_records(&src.identity, Some(&*src));
    let (claimed, mismatched): (Vec<_>, Vec<_>) =
        records.into_iter().partition(crate::support::RecoveryRecord::claimable);
    for r in &mismatched {
        println!("candidate: {}", r.describe());
    }
    let path = match claimed.as_slice() {
        [] => {
            if !mismatched.is_empty() {
                println!(
                    "none of the {} candidate record(s) above belongs to this target — they are left in place",
                    mismatched.len()
                );
            }
            return None;
        }
        // 身份与落点都唯一：按原路径回放
        [one] => one.path().to_path_buf(),
        many => bail_fail(RunOutcome::refused(format!(
            "this target has {} recovery records that would be replayed — `undo` rolls back exactly one and \
             does not pick one for you; release the extra records with `diskedit abandon`, or move one out of \
             the record set by hand: {}",
            many.len(),
            many.iter().map(|r| r.path().display().to_string()).collect::<Vec<_>>().join("; ")
        ))),
    };
    let current = match crate::recovery::ownership_scene(&*src) {
        Ok(current) => current,
        Err(e) => bail_fail(RunOutcome::refused(format!(
            "the target's identity could not be read ({e:?}), so the recovery record's ownership cannot be \
             proven — nothing was read or written; rerun once the target can be probed"
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
            Ok(()) => {
                let journal_record = src.record_journal().map(|j| j.record());
                table_write_done(
                    src,
                    journal_record,
                    "the recorded operation had already committed — its recovery record was released and nothing was \
                     rolled back (verify with: diskedit info)",
                )
            }
            Err(e) => {
                let msg = format!(
                    "the recorded operation had already committed, but its recovery record could not be released ({e}); \
                     the record is left in place"
                );
                // 回读不符是「要删的不是这份记录」的拒绝，不是收尾故障：本次未写目标字节，落 3
                if matches!(e, crate::recovery::RecordJournalError::ReadbackMismatch(_)) {
                    let o = RunOutcome::refused(format!("{msg} — release it with `diskedit abandon`"));
                    // 本分支返回裸退出码，不经过调用方的 report：拒绝理由在这里自己打印，
                    // 否则命令退 3 而 stdout/stderr 全空，用户无从知道为什么
                    return Some(o.report_final().exit_code());
                } else {
                    let o = RunOutcome::partial_postcondition(msg, Vec::new());
                    return Some(o.report_final().exit_code());
                }
            }
        };
        return Some(outcome.exit_code());
    }

    // 准入与执行由恢复层承担：资格、身份、区间、源与盘面核对全部通过才签发许可；
    // 盘面已回到原文时不签发，走零写入收尾
    match crate::recovery::RecoveryEngine::admit_rollback(&*src, record, &path) {
        crate::recovery::Admission::AlreadyReverted(record, path) => {
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
            let record = src.record_journal().map(|j| j.record());
            let outcome = crate::support::settle_layout(outcome, src, record);
            if outcome.is_complete() {
                println!(
                    "the target already held the recorded pre-images — nothing was written back (verify with: diskedit info)"
                );
            }
            Some(outcome.exit_code())
        }
        crate::recovery::Admission::Proceed(provisional, permit) => {
            Some(crate::recovery::execute_rollback(src, provisional, permit))
        }
    }
}
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
        match crate::recovery::archive_torn(path) {
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
        let record = src.record_journal().map(|j| j.record());
        let outcome = crate::support::settle_layout(outcome, src, record);
        return outcome.exit_code();
    }
    // 与全量路径同一豁免：`move` / `copy` 的 preimage 只覆盖表区，撕裂载荷里已经
    // 解出的那部分仍可回放；数据区本就没有记录，不存在"回放到一半"的问题
    if prefix.record.restore_blocked && !crate::recovery::table_only_replay(prefix.record.op) {
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
    crate::recovery::validate_record_ranges(src, &prefix.record.restore, path);
    if let Err(f) = crate::recovery::verify_disk_identity(prefix.record.disk_guid, src, "the recovery record") {
        bail_fail(f);
    }
    if let Err(f) = crate::recovery::verify_record_plan(&*src, &prefix.record.restore) {
        bail_fail(f);
    }
    crate::recovery::replay_record(src, &prefix.record.restore, path);
    let mut outcome = crate::outcome::RunOutcome::applied_with(Vec::new());
    let kept = prefix.kept;
    let total = prefix.total;
    match crate::recovery::archive_torn(path) {
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
    let record = src.record_journal().map(|j| j.record());
    let outcome = crate::support::settle_layout(outcome, src, record);
    outcome.exit_code()
}

/// 旧 journal 不是恢复记录：它早于记录格式，本构建不回放、不迁移、不删除。
/// 落点上只要存在条目（含符号链接与 dangling 链接）就按未识别证据拒绝（3）。
fn refuse_legacy_journal(src: &FileSource) {
    let path = src.identity.journal_path();
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => bail_fail(RunOutcome::no_mutation(format!(
            "cannot determine whether a legacy undo journal exists at {}: {e}",
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
}
