//! undo：回放 journal，撤销本工具的直接写入（分区表与搬移前的原字节）。

use crate::support::*;
use crate::args::Args;
use crate::dev::{Journal, JournalRead, RecoveryData};

pub(crate) const HELP: &str = r#"diskedit undo <TARGET> --yes

  Replay the journal to undo this tool's direct writes (partition table and
  relocated data). Writes made by external FS tools are not undone."#;

/// 挑不出唯一可读 journal 的三种原因——**性质不同，出口码就该不同**，故在这里
/// 分型而不是把文案揉成一个 String 让调用方猜：
/// - `Absent`：落点上根本没有 journal ⇒ 请求与目标现状不匹配（10）
/// - `Unreadable`：文件在而读不出来（CRC/magic/损坏/I-O）⇒ 盘上事实，重试无用（30）；
///   只有 abandon 能释放，提示也指向那里
/// - `Ambiguous`：多份同时可读 ⇒ 不猜（猜错会把历史字节回放到不该回放的盘上）
#[derive(Debug)]
enum PickJournalError {
    Absent,
    Unreadable { detail: String },
    Ambiguous { listed: String },
}

impl std::fmt::Display for PickJournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PickJournalError::Absent => write!(f, "no undo journal on this target"),
            PickJournalError::Unreadable { detail } => write!(f, "the undo journal exists but is unreadable: {detail} — \
                 it cannot be replayed; release it with `diskedit abandon`"),
            PickJournalError::Ambiguous { listed } => write!(f, "multiple journals found for this target — refusing: {listed}"),
        }
    }
}

/// 在候选落点中挑出唯一可读的 journal。
/// 候选列表按"本次命名在前、历史命名在后"给出，缺席是常态而非故障：把它记成错误会让
/// 真正的损伤原因（另一份文件存在但读不出来）被 `No such file or directory` 盖住。
/// 损坏候选**记录后继续**看后面的候选（与 read_checkpoint 同一口径）：一份损坏的历史
/// 命名不该遮住一份完好的本次命名；全部候选都不可读时才把死因如实带回
fn pick_journal(candidates: &[std::path::PathBuf]) -> Result<(std::path::PathBuf, JournalRead), PickJournalError> {
    let mut usable: Vec<(std::path::PathBuf, JournalRead)> = Vec::new();
    let mut damaged: Vec<String> = Vec::new();
    for path in candidates {
        match Journal::read_entries(path) {
            Ok(r) => usable.push((path.clone(), r)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => damaged.push(format!("{} ({e})", path.display())),
        }
    }
    match usable.len() {
        1 => {
            for d in &damaged {
                eprintln!("warning: unreadable journal {d}");
            }
            Ok(usable.remove(0))
        }
        0 if damaged.is_empty() => Err(PickJournalError::Absent),
        0 => Err(PickJournalError::Unreadable { detail: damaged.join("; ") }),
        _ => Err(PickJournalError::Ambiguous {
            listed: usable.iter().map(|(p, _)| p.display().to_string()).collect::<Vec<_>>().join(", "),
        }),
    }
}

/// checkpoint 的历史命名带 GPT Disk GUID，而 GUID 只在表可读时存在。undo 属恢复路径：
/// 表读不出来时要照常工作（该候选缺席即可），故一切失败都降级为 None——判据与
/// `abandon` 完全同源，共用 `legacy_disk_guid`（含降级告警），两处不各写一遍
pub(crate) fn cmd_undo(a: &Args) -> u8 {
    // undo 回放的是整盘 journal：`:N` 指定了也会被静默忽略（与 info 同判据）
    if let Some(n) = a.part {
        bail_fail(Fail::refused(format!("`undo` replays the whole target's journal — drop :{n}")));
    }
    if !a.yes {
        bail_fail(Fail::refused("`undo` overwrites current bytes from journal; pass --yes to confirm"));
    } else {
        let mut src = open_target_owned(a).unwrap_or_else(|f| bail_fail(f));
        // 三种挑不出各自的出口码：缺席 = 请求与现状不符（10）；在而读不出 = 盘上事实，
        // 重试无用且只有 abandon 能释放（30）；歧义同样按拒绝处理（不猜）
        let (p, read) = pick_journal(src.identity.journal_candidates()).unwrap_or_else(|e| bail_fail(match e {
            PickJournalError::Unreadable { .. } => Fail::infra(e.to_string()),
            _ => Fail::refused(e.to_string()),
        }));
        let (entries, tail_incomplete) = match read {
            JournalRead::Complete(v) => (v, false),
            // 尾部未完成的记录：append-only 下那次追加没走完，它对应的写入也就没发生，
            // 丢弃安全；前面的完整前缀照常回放（严格契约仍守：每条都过了 CRC）
            JournalRead::TruncatedTail(v) => (v, true),
        };
        if entries.is_empty() {
            // 空 journal 与"目标上留着未收尾的 checkpoint"是两件事：后者描述的是另一族作业
            // 的进度（例如搬移搬到一半），undo 回放不了它，也不能假装目标干净——它会让后续
            // resize 被判成 Divergent 而拒绝，用户看到的是"什么也没做却被拒绝"
            let leftover = src.identity.checkpoint_candidates(legacy_disk_guid(&src));
            let stale: Vec<String> = leftover.iter().filter(|p| p.exists()).map(|p| p.display().to_string()).collect();
            if stale.is_empty() {
                bail_fail(Fail::refused("nothing to undo (journal is empty)".to_string()));
            }
            bail_fail(Fail::refused(format!(
                "the undo journal is empty, but this target still has an unfinished checkpoint ({}); undo cannot release it — \
                 re-run the command that started that job to resume it to completion",
                stale.join(", ")
            )));
        }
        let n = entries.len();
        if tail_incomplete {
            eprintln!(
                "warning: the journal ends with an incomplete record (an interrupted append, or that record was damaged) — \
                 replaying the {n} complete record(s) before it; anything recorded after that point cannot be undone"
            );
        }
        // 含**不可回滚写入**的 journal 不可回滚：数据搬移的字节、外部 FS 工具的写入
        // （resize2fs/mkswap/mkfs…）与内核侧表写入都按设计不入 journal，只回滚表项会留下
        // 表与盘上内容自相矛盾的布局（例如 FS 自述尺寸 > 分区尺寸），故显式拒绝而非假回滚。
        // 屏障记着是哪一类越过了这条线，理由因此可以直接说给用户听
        if let Some(m) = entries.iter().find_map(|r| match r.recovery {
            RecoveryData::Barrier => Some(r.mutation),
            RecoveryData::PreImage { .. } => None,
        }) {
            bail_fail(Fail::refused(format!(
                // 只陈述 undo 做不到什么、以及谁做得到。"重跑原命令续跑"不能写在这里：
                // 并非所有不可回滚的场景都有 ckpt 可续（`copy` 就没有），那句判断
                // 属于 transaction 的出路分流（见 `TransactionManager::busy`）
                "this journal records a non-reversible mutation ({}), so rolling back only the table would leave the layout \
                 contradicting the on-disk content; restore from backup, or release the transaction with `diskedit abandon`",
                m.describe()
            )));
        }
        for rec in entries.iter().rev() {
            let RecoveryData::PreImage { off, bytes, after } = &rec.recovery else { continue };
            // 分叉核对：盘上当前字节须与这条记录的写后内容一致（正常现场），或与原文
            // 一致（上一次 undo 中断留下的已回滚态——重放同一原文幂等无害）。
            // 其余值说明该写入之后有外部改动动过同一段字节（journal 不挡第三方），
            // 回放会把它覆盖掉
            let mut now = vec![0u8; bytes.len()];
            match src.read_at(*off, &mut now) {
                Ok(()) if now == *after || now == *bytes => {}
                Ok(_) => bail_fail(Fail::refused(format!(
                    "target no longer matches the journaled write at offset {off} — the disk changed outside this tool after that write; \
                     replaying would overwrite the foreign edit (release the transaction with `diskedit abandon` if the rollback is no longer wanted)"
                ))),
                Err(e) => bail_fail(Fail::refused(format!(
                    "cannot verify the journaled write at offset {off} against the target: {e} — refusing to replay over an unverifiable region"
                ))),
            }
            if let Err(e) = src.write_at(*off, bytes) {
                // journal 保留在原地：可重试 undo。回放进行到一半才失败——前面的条目
                // 已经写入，"确定未写盘"的断言不成立，归 Failed（可能已改变）
                bail_fail(Fail::failed(format!("undo write failed at offset {off}: {e} (journal kept, retry)")));
            }
            // 逐条落盘再进下一条：掉电重试时，已回滚条目在盘上稳定停在原文（通过核对），
            // 进度精确到条；整场回放攒到最后一次 sync 的话，一次掉电可撕裂任意多条，
            // 撕裂字节与外部改动无法区分，重试只能被拒
            if let Err(e) = src.sync_data() {
                bail_fail(Fail::failed(format!("undo sync failed at offset {off}: {e} (journal kept, retry)")));
            }
        }
        // undo 的契约是"盘确定回到写入前状态"：sync 失败意味着回滚可能未落盘，
        // 不能报成功——那会让用户以为已经回滚。回放本身已完成，但持久性无法断言，
        // 同样只承诺"可能已改变，需验证"
        src.sync_all().unwrap_or_else(|e| {
            bail_fail(Fail::failed(format!("undo wrote the journal back but sync failed: {e} — rollback may not be durable, verify before retrying")))
        });
        crate::dev::warn_if_remove_failed(&p);
        // 事务的恢复状态随回滚一并释放：这份 journal 记下的表写入已经全部回退，而 checkpoint
        // 描述的是同一个事务的进度——留下来只会描述一个已被回滚掉的世界，让后续 resize 被判成
        // Divergent 而永久拒绝（"什么也没做却被拒绝"）。两族作业不会交叉：目标上/journal 存在
        // 期间，另一族作业根本起不来（见 prepare_* 的槽位判定）
        for ckpt in src.identity.checkpoint_candidates(legacy_disk_guid(&src)) {
            crate::dev::warn_if_remove_failed(&ckpt);
        }
        table_write_done(&src, &format!("undone {n} journal entries (verify with: diskedit info {})", a.target))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::let_underscore_must_use)] // 清理临时文件有意忽略失败
    use super::{pick_journal, PickJournalError};
    use crate::dev::Journal;
    use std::path::PathBuf;

    fn tmp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("diskedit_undo_{tag}_{}.journal", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// 上次 undo 中断的现场（部分条目已回滚、其余未动）必须能续跑：核对接受两种
    /// 历史——写后内容（未动）与原文（自己上次的回滚进度）。只认写后内容会把
    /// "journal kept, retry" 的承诺落空：中断的回滚永远续不上，只能 abandon 成
    /// 半回滚混合态。注意 cmd_undo 的失败出口是 process::exit，本测试只覆盖成功路径
    #[test]
    fn interrupted_undo_replay_resumes_over_its_own_rollback() {
        let img = tmp_path("resume_img");
        let mut jpath = img.clone().into_os_string();
        jpath.push(".diskedit.journal");
        let journal = std::path::PathBuf::from(jpath);
        let _ = std::fs::remove_file(&journal);
        {
            let mut j = Journal::open(&journal).unwrap();
            j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
            j.record(100, &[0xCC; 4], &[0xDD; 4]).unwrap();
        }
        // 逆序回放中断后的盘面：offset 100 已回滚（进度条），offset 0 未动
        let mut disk = vec![0u8; 1024];
        disk[0..4].copy_from_slice(&[0xBB; 4]);
        disk[100..104].copy_from_slice(&[0xCC; 4]);
        std::fs::write(&img, &disk).unwrap();

        let a = crate::args::Args {
            target: img.to_string_lossy().into_owned(),
            part: None, grow: None, start: None, end: None, size: None,
            fs: None, name: None, type_guid: None, table: None,
            yes: true, online: false, random: false, sector_size: None,
            align: String::new(), chunk_mib: 0, grow_to_end: false,
            allow_move: false, no_fs: false, grow_lv: false, start_end: false, lv: None,
            pos: Vec::new(), seen: Vec::new(),
        };
        let code = super::cmd_undo(&a);
        assert_eq!(code, 0, "undo must resume over its own interrupted progress");
        let now = std::fs::read(&img).unwrap();
        assert_eq!(&now[0..4], &[0xAA; 4], "the untouched entry must be rolled back");
        assert_eq!(&now[100..104], &[0xCC; 4], "the rolled-back entry stays at the original bytes");
        assert!(!journal.exists(), "a completed undo must drop the journal");

        let mut lpath = img.clone().into_os_string();
        lpath.push(".diskedit.lock");
        let _ = std::fs::remove_file(&img);
        let _ = std::fs::remove_file(std::path::PathBuf::from(lpath));
    }

    /// 候选缺席不是故障：它不得盖住另一份候选的真实损伤原因。
    /// 两份候选里"第一份不存在、第二份是陌生文件"是最常见的现场（历史命名那份通常不存在），
    /// 报出 No such file or directory 等于让用户去查一个根本不是原因的路径
    #[test]
    fn absent_candidate_does_not_mask_a_damaged_one() {
        let missing = tmp_path("missing");
        let foreign = tmp_path("foreign");
        std::fs::write(&foreign, b"not-a-journal").unwrap();
        let e = pick_journal(&[missing.clone(), foreign.clone()]).err().unwrap();
        // 在而读不出 ⇒ Unreadable（下游归 Infra + abandon 提示），且死因里不含缺席路径
        assert!(matches!(e, PickJournalError::Unreadable { .. }), "{e}");
        assert!(e.to_string().contains(&foreign.display().to_string()), "{e}");
        assert!(!e.to_string().contains(&missing.display().to_string()), "{e}");

        // 全部候选都不存在 → Absent（请求与现状不符，10）
        let e = pick_journal(std::slice::from_ref(&missing)).err().unwrap();
        assert!(matches!(e, PickJournalError::Absent), "{e}");

        // 唯一存在且可读的候选被选中（缺席的那份不影响）
        let ok = tmp_path("ok");
        {
            let mut j = Journal::open(&ok).unwrap();
            j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
        }
        let (p, _) = pick_journal(&[missing, ok.clone()]).unwrap();
        assert_eq!(p, ok);

        let _ = std::fs::remove_file(&ok);
        let _ = std::fs::remove_file(&foreign);
    }

    /// 两份候选同时可读 → Ambiguous：不猜（回放错一份就是把历史字节写到不该写的盘上），
    /// 错误文案列出两份路径，供 abandon 逐一定位
    #[test]
    fn two_readable_candidates_are_ambiguous_not_silently_picked() {
        let a = tmp_path("amb_a");
        let b = tmp_path("amb_b");
        for p in [&a, &b] {
            let mut j = Journal::open(p).unwrap();
            j.record(0, &[0xAA; 4], &[0xBB; 4]).unwrap();
        }
        let e = pick_journal(&[a.clone(), b.clone()]).err().unwrap();
        assert!(matches!(e, PickJournalError::Ambiguous { .. }), "{e}");
        assert!(e.to_string().contains(&a.display().to_string()), "{e}");
        assert!(e.to_string().contains(&b.display().to_string()), "{e}");

        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }
}