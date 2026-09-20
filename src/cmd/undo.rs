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
            let RecoveryData::PreImage { off, bytes } = &rec.recovery else { continue };
            if let Err(e) = src.write_at(*off, bytes) {
                // journal 保留在原地：可重试 undo
                bail_fail(Fail::infra(format!("undo write failed at offset {off}: {e} (journal kept, retry)")));
            }
        }
        // undo 的契约是"盘确定回到写入前状态"：sync 失败意味着回滚可能未落盘，
        // 不能报成功——那会让用户以为已经回滚
        src.sync_all().unwrap_or_else(|e| {
            bail_fail(Fail::infra(format!("undo wrote the journal back but sync failed: {e} — rollback may not be durable, verify before retrying")))
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
            j.record(0, &[0xAA; 4]).unwrap();
        }
        let (p, _) = pick_journal(&[missing, ok.clone()]).unwrap();
        assert_eq!(p, ok);

        let _ = std::fs::remove_file(&ok);
        let _ = std::fs::remove_file(&foreign);
    }
}