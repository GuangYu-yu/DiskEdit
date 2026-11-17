//! recover：回滚恢复记录里记下的原始字节（分区表等直接写入）。

use crate::cli::RecoverRequest;
use crate::dev::WriteSource;
use crate::support::*;

pub(crate) const DETAILS: &str = r#"  Finish with this target's recovery record. Two outcomes share one command:

  replay    (default) put back the bytes the recorded operation overwrote —
            partition-table entries and the in-place filesystem metadata it
            changed (NTFS backup sector pointer). Relocated data cannot be
            rolled back, and writes made by external FS tools are not undone.
            --replay-table-only replays only the recorded table regions of a
            move whose data has already moved.
  --release drop the recovery files, keeping the writes they describe. This
            does not need the record to be readable, and it does not replay
            anything. When the scene still holds at least one range a replay
            could restore — or the payload is damaged, so the count is
            unknown — the release is refused until --discard-replay is given;
            that flag confirms giving up a replayable range, it is not
            another spelling of --yes.

  Examples:
    diskedit recover /dev/sdb --yes
    diskedit recover /dev/sdb --release --yes"#;


fn replay_record(src: &mut WriteSource, permit_move: bool) -> Option<u8> {
    // 记录不按单一派生落点认：身份漂移或目录被改名后，派生路径找不到它。枚举把
    // 身份不符/无法比对的候选一并列出，但只认领身份严格相符的那些
    let records = match crate::support::active_recovery_records(&src.identity, Some(&*src)) {
        Ok(records) => records,
        Err(why) => bail_fail(RunOutcome::refused(format!(
            "the recovery scene of this target cannot be enumerated: {why} — nothing was read or written; \
             rerun once the record set is readable"
        ))),
    };
    let (claimed, mismatched): (Vec<_>, Vec<_>) =
        records.into_iter().partition(crate::support::RecoveryRecord::claimable);
    for r in &mismatched {
        println!("candidate: {}", r.describe());
    }
    let path = match claimed.as_slice() {
        [] => {
            if !mismatched.is_empty() {
                println!(
                    "none of the {} candidate record(s) above can be claimed for this target — they are left \
                     in place",
                    mismatched.len()
                );
            }
            return None;
        }
        // 身份与落点都唯一：按原路径回放
        [one] => one.path().to_path_buf(),
        many => bail_fail(RunOutcome::refused(format!(
            "this target has {} recovery records that would be replayed — `recover` rolls back exactly one and \
             does not pick one for you; release the extra records with `diskedit recover --release`, or move one out of \
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
        Err(e) => bail_fail(RunOutcome::refused(format!(
            "the recovery record cannot be read ({e}) — nothing was read or written; release it with `diskedit recover --release`"
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
                 release it with `diskedit recover --release`",
                path.display()
            )))
        }
        other => {
            // 载荷未通过完整校验：记录里的字节无法逐项验证，任何区间都不写回，
            // 记录留在原地等人工处理或显式 `recover --release`
            bail_fail(RunOutcome::refused(format!(
                "the recovery record at {} is not replayable ({other:?}) — nothing was read or written; \
                 it is left in place, release it with `diskedit recover --release`",
                path.display()
            )))
        }
    };
    if record.committed() {
        // 提交后未清理：目标字节已定型，本次运行只释放记录、不回滚任何区间。
        // 记录文件被移走本身是持久状态变化，故不是 NoMutation（4），而是 Partial（5）
        // audit-allow: C-1
        let outcome = match crate::recovery::settle_companion(&path, &record, false) {
            Ok(()) => RunOutcome::partial_records_changed(
                "the recorded operation had already committed — no recorded range was rolled back; the recovery \
                 record was removed (verify with: diskedit info)",
            ),
            Err(e) => {
                // 回读不符是「要删的不是这份记录」的拒绝，不是收尾故障：本次未写目标字节，落 3
                if matches!(e, crate::recovery::RecordJournalError::ReadbackMismatch(_)) {
                    let msg = format!(
                        "the recorded operation had already committed, but its recovery record could not be \
                         released ({e}); the record is left in place"
                    );
                    let o = RunOutcome::refused(format!("{msg} — release it with `diskedit recover --release`"));
                    // 本分支返回裸退出码，不经过调用方的 report：拒绝理由在这里自己打印，
                    // 否则命令退 3 而 stdout/stderr 全空，用户无从知道为什么
                    return Some(o.report_final().exit_code());
                }
                // 删除已发生、目录 fsync 未确认：不陈述"记录还在"，也不指向必然的 `recover --release`
                let msg = if e.record_still_on_disk() {
                    format!(
                        "the recorded operation had already committed, but its recovery record could not be \
                         released ({e}); the record is left in place"
                    )
                } else {
                    format!("the recorded operation had already committed — {e}")
                };
                RunOutcome::partial_postcondition(msg, Vec::new())
            }
        };
        let outcome = crate::support::settle_layout(outcome, &*src, None);
        return Some(outcome.exit_code());
    }

    // 准入与执行由恢复层承担：资格、身份、区间、源与盘面核对全部通过才签发许可；
    // 盘面已回到原文时不签发，走零写入收尾
    match crate::recovery::RecoveryEngine::admit_rollback(&*src, record, &path, permit_move) {
        crate::recovery::Admission::AlreadyReverted(record, path) => {
            let restore_empty = record.restore.is_empty();
            let mut outcome = crate::outcome::RunOutcome::applied_with(Vec::new());
            // audit-allow: C-2
            if let Err(e) = crate::recovery::settle_companion(&path, &record, true) {
                if matches!(e, crate::recovery::RecordJournalError::ReadbackMismatch(_)) {
                    bail_fail(RunOutcome::refused(format!(
                        "the recovery record read back before deletion is not the one verified ({e}); \
                         the record is left in place — release it with `diskedit recover --release`"
                    )));
                }
                // 目录项已移除、持久化未确认：只说这一事实，不承诺记录还在
                let msg = if e.record_still_on_disk() {
                    format!("the recovery record could not be released ({e}); release it with `diskedit recover --release`")
                } else {
                    format!("{e}")
                };
                outcome = outcome.degrade_partial(msg);
            }
            let record = src.record_journal().map(|j| j.record());
            let outcome = crate::support::settle_layout(outcome, src, record);
            if outcome.is_complete() {
                if restore_empty {
                    // audit-allow: N-1
                    let msg = "the recovery record held no written ranges — nothing was written back (verify with: diskedit info)";
                    println!("{msg}");
                } else {
                    // audit-allow: N-2
                    let msg = "the target already held the recorded pre-images — nothing was written back (verify with: diskedit info)";
                    println!("{msg}");
                }
            }
            Some(outcome.exit_code())
        }
        crate::recovery::Admission::Proceed(provisional, permit) => {
            Some(crate::recovery::execute_rollback(src, provisional, permit))
        }
    }
}

/// `recover` 认领的是载荷声明为本目标的记录：不按单一派生落点找，兄弟目录里的落点同样
/// 参与枚举（见 `replay_record`）。记录缺席是常态，按拒绝（3）上报
pub(crate) fn cmd_recover(a: &RecoverRequest) -> u8 {
    // `recover` 回滚的是整盘记录：与 info 同判据，`:` 后给了编号即拒绝，不静默忽略
    if let Some(n) = a.part {
        bail_fail(RunOutcome::refused(format!("`recover` rolls back the whole target's recovery record — drop :{n}")));
    }
    // 回放类命令的载体必须是整盘：记录记的是整盘偏移，落到分区节点上会把
    // 别处的字节写进该分区
    refuse_partition_node_target("recover", &a.target);
    let mut src = open_target_owned(a).unwrap_or_else(|f| bail_fail(f));
    // 回放写回的是整盘表字节：目标仍在被使用时回放，会让内核分区视图与盘面
    // 从"分叉"变成"按记录里的旧布局分叉"。闸门在任何写回与记录处理之前
    if let Err(f) = crate::fsops::ensure_table_write_idle(&src) {
        bail_fail(f);
    }
    if let Some(code) = replay_record(&mut src, a.replay_table_only) {
        return code;
    }
    bail_fail(RunOutcome::refused(format!("no recovery record on {} — nothing to recover", a.target)))
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
            block_identity: None,
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
        let p = dir.join("disk.img");
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
