//! undo：回放 journal，撤销本工具的直接写入（分区表与搬移前的原字节）。

use crate::support::*;
use crate::args::Args;
use crate::dev::{Journal, JournalRead};

pub(crate) const HELP: &str = r#"diskedit undo <TARGET> --yes

  Replay the journal to undo this tool's direct writes (partition table and
  relocated data). Writes made by external FS tools are not undone."#;

/// 在候选落点中挑出唯一可读的 journal。
/// 候选列表按"本次命名在前、历史命名在后"给出，缺席是常态而非故障：把它记成错误会让
/// 真正的损伤原因（另一份文件存在但读不出来）被 `No such file or directory` 盖住。
/// 两份候选同时可读即报歧义——猜错会把历史字节回放到不该回放的盘上
fn pick_journal(candidates: &[std::path::PathBuf]) -> Result<(std::path::PathBuf, JournalRead), String> {
    let mut usable: Vec<(std::path::PathBuf, JournalRead)> = Vec::new();
    let mut first_err: Option<String> = None;
    for path in candidates {
        match Journal::read_entries(path) {
            Ok(r) => usable.push((path.clone(), r)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                first_err.get_or_insert_with(|| e.to_string());
            }
        }
    }
    match usable.len() {
        0 => Err(format!("no usable journal: {}", first_err.unwrap_or_else(|| "not found".to_string()))),
        1 => Ok(usable.remove(0)),
        _ => {
            let listed: Vec<String> = usable.iter().map(|(p, _)| p.display().to_string()).collect();
            Err(format!("multiple journals found for this target — refusing: {}", listed.join(", ")))
        }
    }
}

pub(crate) fn cmd_undo(a: &Args) -> u8 {
    if !a.yes {
        bail_fail(Fail::refused("`undo` overwrites current bytes from journal; pass --yes to confirm"));
    } else {
        let mut src = open_target(a).unwrap_or_else(|f| bail_fail(f));
        let (p, read) = pick_journal(src.identity.journal_candidates()).unwrap_or_else(|m| bail_fail(Fail::refused(m)));
        let (entries, tail_incomplete) = match read {
            JournalRead::Complete(v) => (v, false),
            // 尾部未完成的记录：append-only 下那次追加没走完，它对应的写入也就没发生，
            // 丢弃安全；前面的完整前缀照常回放（严格契约仍守：每条都过了 CRC）
            JournalRead::TruncatedTail(v) => (v, true),
        };
        if entries.is_empty() {
            bail_fail(Fail::refused("nothing to undo (journal is empty)".to_string()));
        }
        let n = entries.len();
        if tail_incomplete {
            eprintln!(
                "warning: the journal ends with an incomplete record (an interrupted append, or that record was damaged) — \
                 replaying the {n} complete record(s) before it; anything recorded after that point cannot be undone"
            );
        }
        // 含搬移的 journal 不可回滚：数据字节按设计不入 journal（前向恢复、无回滚），
        // 只回滚表项会留下表与数据不一致的布局，必须显式拒绝而非给出假回滚
        if entries.iter().any(|(off, _)| *off == Journal::MOVED_MARKER) {
            bail_fail(Fail::refused("journal covers a partition relocation — moved/copied data is not journaled by design, so undo cannot revert it (re-run the original command to resume, or restore from backup)".to_string()));
        }
        for (off, data) in entries.iter().rev() {
            if let Err(e) = src.write_at(*off, data) {
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
        table_write_done(&src, &format!("undone {n} journal entries (verify with: diskedit info {})", a.target))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::let_underscore_must_use)] // 清理临时文件有意忽略失败
    use super::pick_journal;
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
        assert!(e.contains("no usable journal"), "{e}");
        assert!(!e.contains("No such file"), "absence must not mask the real cause: {e}");

        // 全部候选都不存在 → 仍须给出"找不到"这一结论
        let e = pick_journal(std::slice::from_ref(&missing)).err().unwrap();
        assert!(e.contains("not found"), "{e}");

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