//! undo：回放 journal，撤销本工具的直接写入（分区表与搬移前的原字节）。

use crate::support::*;
use crate::args::Args;
use crate::dev::{Journal, JournalRead};

pub(crate) const HELP: &str = r#"diskedit undo <TARGET> --yes

  Replay the journal to undo this tool's direct writes (partition table and
  relocated data). Writes made by external FS tools are not undone."#;

pub(crate) fn cmd_undo(a: &Args) -> u8 {
    if !a.yes {
        eprintln!("refused: `undo` overwrites current bytes from journal; pass --yes to confirm");
        EXIT_REFUSED
    } else {
        let mut src = open_target(a).unwrap_or_else(|(c, m)| bail(c, m));
        let p = journal_path(&src.path, src.is_block);
        let (entries, tail_incomplete) = match Journal::read_entries(&p) {
            Ok(JournalRead::Complete(v)) => (v, false),
            // 尾部未完成的记录：append-only 下那次追加没走完，它对应的写入也就没发生，
            // 丢弃安全；前面的完整前缀照常回放（严格契约仍守：每条都过了 CRC）
            Ok(JournalRead::TruncatedTail(v)) => (v, true),
            Err(e) => bail(EXIT_REFUSED, format!("no usable journal: {e}")),
        };
        if entries.is_empty() {
            bail(EXIT_REFUSED, "nothing to undo (journal is empty)".to_string());
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
            bail(EXIT_REFUSED, "refused: journal covers a partition relocation — moved/copied data is not journaled by design, so undo cannot revert it (re-run the original command to resume, or restore from backup)".to_string());
        }
        for (off, data) in entries.iter().rev() {
            if let Err(e) = src.write_at(*off, data) {
                // journal 保留在原地：可重试 undo
                bail(EXIT_INFRA, format!("undo write failed at offset {off}: {e} (journal kept, retry)"));
            }
        }
        // undo 的契约是"盘确定回到写入前状态"：sync 失败意味着回滚可能未落盘，
        // 不能报成功——那会让用户以为已经回滚
        src.sync_all().unwrap_or_else(|e| {
            bail(EXIT_INFRA, format!("undo wrote the journal back but sync failed: {e} — rollback may not be durable, verify before retrying"))
        });
        crate::dev::warn_if_remove_failed(&p);
        table_write_done(&src, &format!("undone {n} journal entries (verify with: diskedit info {})", a.target))
    }
}