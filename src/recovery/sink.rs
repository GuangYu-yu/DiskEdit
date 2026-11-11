//! 记录驱动的进度 sink：把执行器报出的搬移检查点与表步完成写进恢复记录。
//!
//! 只落进度：表内容与整份替换归命令层，本类型不写表字节，也不替换记录。

use crate::dev::WriteSource;
use crate::execution::{StepSink, TransferState};
use crate::outcome::RunOutcome;

/// 把 [`TransferState`] 落进恢复记录。
///
/// 记录由命令层在写盘前建好并由 [`WriteSource`] 持有；本类型不建记录、不做整份替换。
/// 首个 [`StepSink::transfer_checkpoint`] 之前，调用方必须先用 `bind_relocation` 把完整
/// 条目列表与块字节写进记录：记录没有搬移计划时 `record_transfer_checkpoint` 拒绝推进
/// 条目游标，两个游标都只接受前进。
///
/// 过程提示的输出通道由调用方给：`resize` 经自己的 logger（文本/JSON 各有写法），
/// `move` 直接打印到标准输出。
pub(crate) struct RecordSink<'a> {
    log: Option<&'a mut dyn FnMut(&str)>,
}

impl<'a> RecordSink<'a> {
    /// 提示走调用方的日志器
    pub(crate) fn with_log(log: &'a mut dyn FnMut(&str)) -> Self {
        Self { log: Some(log) }
    }

    /// 没有日志器时按文本打到标准输出
    pub(crate) fn quiet() -> Self {
        Self { log: None }
    }
}

impl StepSink for RecordSink<'_> {
    fn transfer_checkpoint(
        &mut self,
        src: &mut WriteSource,
        state: &TransferState,
    ) -> Result<(), RunOutcome> {
        let journal = src.record_journal_mut().ok_or_else(|| {
            RunOutcome::partial(
                "the target carries no recovery record — the transfer may be incomplete",
            )
        })?;
        // 单次 persist 同时落条目游标与 progress；缩容状态沿用记录里的值
        journal
            .record_transfer_checkpoint(
                Some((state.entry_index, state.chunks_in_entry)),
                state.completed_steps,
                state.transfer_cursor,
            )
            .map_err(|e| {
                RunOutcome::partial(format!(
                    "{} — the transfer may be incomplete",
                    e.record_failure("the transfer checkpoint")
                ))
            })
    }

    fn table_step_done(
        &mut self,
        src: &mut WriteSource,
        _step: usize,
        state: &TransferState,
    ) -> Result<(), RunOutcome> {
        let journal = src.record_journal_mut().ok_or_else(|| {
            RunOutcome::partial(
                "the target carries no recovery record — the table step may not be durably recorded",
            )
        })?;
        // 执行器在调用前已把 entry_index 加一、chunks_in_entry 归零、completed_steps 加一，
        // 这里只持久化这三个值，不再加一
        let reloc = journal
            .relocation()
            .map(|_| (state.entry_index, state.chunks_in_entry));
        journal
            .record_transfer_checkpoint(
                reloc,
                state.completed_steps,
                state.transfer_cursor,
            )
            .map_err(|e| {
                RunOutcome::partial(format!(
                    "{} — the on-disk state may have changed",
                    e.record_failure("the table step completion")
                ))
            })
    }

    fn fs_step_done(
        &mut self,
        src: &mut WriteSource,
        _step: usize,
        state: &TransferState,
    ) -> Result<(), RunOutcome> {
        let journal = src.record_journal_mut().ok_or_else(|| {
            RunOutcome::partial(
                "the target carries no recovery record — the filesystem step may not be durably recorded",
            )
        })?;
        // 文件系统步骤不动搬移游标（reloc 传 None），只落已完成的步数：外部工具已退出，
        // 这一步做过与否不能只留在内存里
        journal
            .record_transfer_checkpoint(None, state.completed_steps, state.transfer_cursor)
            .map_err(|e| {
                RunOutcome::partial(format!(
                    "{} — the on-disk state may have changed",
                    e.record_failure("the filesystem step completion")
                ))
            })
    }

    fn note(&mut self, msg: &str) {
        match self.log.as_mut() {
            Some(log) => log(msg),
            None => println!("{msg}"),
        }
    }
}
