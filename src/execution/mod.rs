//! 执行编排：已确认计划的步骤分派。
//!
//! 入口 [`execute_plan`] 只收 [`ConfirmedPlan`]。**计划步骤**的写盘只有这一条路径：
//! 搬移、复制、表提交、换区重建、文件系统扩缩与属性改写都从这里走，命令层不再自己
//! 调工具或写表。计划之外另有三类补写：
//! - `cmd/layout.rs` 两处在**已提交态续跑**时直接调 `movepart::fix_ntfs_hidden_sectors`
//!   改引导扇区（`resize-part` 起点变化、`move` 已提交条目的收尾）。同名函数在本模块的
//!   `fix_hidden_sectors`（表提交之后）属于计划步骤，不是链外写入。
//! - `cmd/undo` 的回退方向回放。
//! - `fsops::wipe_zero` 在 mkfs 前擦除旧签名。
//!
//! 三者不写分区表项，也不经计划步骤，故「全仓唯一」的说法不成立。
//!
//! 每个步骤要么完成、要么给出"后置条件未达成"（返回 [`StepFlow::Stopped`] 并留下待办），
//! 要么按"首写之前 / 之后"分类失败。预扫描在第一个目标字节写入之前跑完：执行器缺席、
//! 未装配的表步骤、不合法的搬移形状都在那里拒绝。

mod copy;
mod fs;
mod mkfs;
mod relocate;
mod set;
mod swap;
mod table;

use self::copy::CopyPartition;
use self::fs::FsResize;
use self::mkfs::FsCreate;
use self::relocate::Relocate;
use self::set::FsSet;
use self::swap::SwapRebuild;
use crate::dev::WriteSource;
use crate::outcome::{Pending, RunOutcome};
use crate::planner::chunk::move_chunk_layout;
use crate::planner::{ConfirmedPlan, PlanStep};
use crate::recovery::{RelocEntry, Relocation};
use crate::table::TableStep;

/// 搬移进度的内存载体：条目内块位置、全局块游标与已完成步骤数都从这里读写。
///
/// 续跑时由恢复记录注入（见 [`journal_state`]），执行过程中只前进。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TransferState {
    pub entry_index: u32,
    pub chunks_in_entry: u64,
    pub completed_steps: u32,
    /// 全局累计块数，跨条目只增不减
    pub transfer_cursor: u64,
}

impl TransferState {
    /// 本计划此前是否已经写过目标字节：已有块进度、更早的条目已提交或已有步骤完成。
    /// 续跑时这些字段由恢复记录注入。
    pub(crate) fn wrote_target(&self) -> bool {
        self.chunks_in_entry > 0 || self.entry_index > 0 || self.completed_steps > 0
    }
}

/// 执行过程中的持久化回调。
///
/// 进度落盘的整份替换由 sink 实现方完成：[`execute_plan`] 只在约定的时点调用
/// [`Self::transfer_checkpoint`]、[`Self::table_step_done`] 与
/// [`Self::fs_step_done`]，自身不写记录。
///
/// 实现方在方法内部用 `src.record_journal_mut()` 短借用记录并立即归还，不得跨调用
/// 持有。[`Self::table_step_done`] 与 [`Self::fs_step_done`] 收到的
/// `state.entry_index` 已经是推进后的值，实现方只落盘，不再加一。
pub(crate) trait StepSink {
    fn transfer_checkpoint(
        &mut self,
        src: &mut WriteSource,
        state: &TransferState,
    ) -> Result<(), RunOutcome>;

    fn table_step_done(
        &mut self,
        src: &mut WriteSource,
        step: usize,
        state: &TransferState,
    ) -> Result<(), RunOutcome>;

    /// 文件系统步骤（`FsResize`/`FsCreate`/`FsSet`）完成后的落盘时点：
    /// 工具步骤在退出后再替换一次记录。进入工具之前的落盘由屏障承担
    fn fs_step_done(
        &mut self,
        src: &mut WriteSource,
        step: usize,
        state: &TransferState,
    ) -> Result<(), RunOutcome>;

    /// 过程提示：逐条目落位、块布局降级这类"用户该看到但不改变结论"的信息。
    /// 输出通道归实现方——命令层的日志格式（文本 / JSON）不能由执行器决定
    fn note(&mut self, _msg: &str) {}
}

/// 不落进度的 sink：没有搬移计划的步骤不需要记录任何游标。
pub(crate) struct NoProgress;

impl StepSink for NoProgress {
    fn transfer_checkpoint(
        &mut self,
        _src: &mut WriteSource,
        _state: &TransferState,
    ) -> Result<(), RunOutcome> {
        Ok(())
    }

    fn table_step_done(
        &mut self,
        _src: &mut WriteSource,
        _step: usize,
        _state: &TransferState,
    ) -> Result<(), RunOutcome> {
        Ok(())
    }

    fn fs_step_done(
        &mut self,
        _src: &mut WriteSource,
        _step: usize,
        _state: &TransferState,
    ) -> Result<(), RunOutcome> {
        Ok(())
    }
}

/// 一份计划的执行结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepFlow {
    /// 全部步骤执行完毕：命令级收尾（扩容、FS、LVM）可以继续
    Complete,
    /// 计划在某一步之后停下：那一步留下了待办，后续步骤没有执行。
    /// 记录必须保留——重跑从停下那一处继续
    Stopped,
}

/// 预扫描后的执行单元：`Transfer` 是「搬数据 + 提交表」的一个条目组，`Copy` 是
/// 「复制数据 + 提交副本条目」的一对，`TableOnly` 是不带搬移的表步骤，`FsResize` 是
/// 写表之后的文件系统步骤，`FsCreate` 是在分区上建立文件系统，`FsSet` 是改写文件系统
/// 内部属性。
enum PreparedStep<'a> {
    /// 一条搬移 + 提交它的表步骤；换区条目在表提交之后多一步重建（`swap`）
    Transfer { reloc: Relocate, commit: &'a TableStep, swap: Option<SwapRebuild> },
    Copy { copy: CopyPartition, commit: &'a TableStep },
    TableOnly(&'a TableStep),
    FsResize(FsResize),
    FsCreate(FsCreate),
    FsSet(FsSet),
}

/// 执行一份已确认的计划。
///
/// 整份计划先扫描一遍再开始写：执行器缺席的步骤、未装配的表步骤、不合法的搬移形状与
/// 参数都在首次目标写入之前拒绝。退出码 1 限定在写目标字节之前，若边执行边
/// 发现缺席，前面已写下的步骤会让这个码失去依据；未装配的表步骤同理，无法完成
/// 写前比对。
///
/// `chunk_bytes` 只对含 `Relocate` / `CopyPartition` 的计划有意义：数据块的字节数必须由
/// 调用方给出并在 1 MiB..=64 MiB 的扇区整数倍内，本函数不读 [`DEFAULT_CHUNK_BYTES`]。
pub(crate) fn execute_plan(
    src: &mut WriteSource,
    plan: &ConfirmedPlan,
    chunk_bytes: Option<u64>,
    sink: &mut dyn StepSink,
    pending: &mut Vec<Pending>,
) -> Result<StepFlow, RunOutcome> {
    let sector_size = plan.plan().sector_size().get();
    let record = src.record_journal().and_then(|j| j.relocation());
    let steps = prepare_steps(src, plan, chunk_bytes, sector_size, record)?;
    verify_first_table(src, &steps)?;
    let mut state = journal_state(src)?;
    // 只有搬移计划才带"搬完之后的那一次扩容"；delete/add/set 的单表步与它无关，
    // 不能让测试注入点在那些命令上误触发
    let has_transfer = steps
        .iter()
        .any(|s| matches!(s, PreparedStep::Transfer { .. }));
    let mut entry = 0u32;
    let mut grow_faulted = false;
    for (index, step) in steps.into_iter().enumerate() {
        match step {
            PreparedStep::Transfer { reloc, commit, swap } => {
                // 记录里已提交的条目整组跳过：数据与表都不再碰
                if entry < state.entry_index {
                    entry += 1;
                    continue;
                }
                if swap.is_none() {
                    let chunk = chunk_bytes
                        .expect("prepare_steps refuses a relocation plan without a chunk size");
                    relocate::execute(src, reloc, sector_size, chunk, &mut state, sink)
                        .map_err(|e| e.context(&format!("plan step {}", index + 1)))?;
                }
                crate::movepart::fault_before_entry_commit();
                table::execute(src, commit, state.wrote_target())
                    .map_err(|e| e.context(&format!("plan step {}", index + 1)))?;
                // 换区条目的表项刚落位、内容尚未重建：重放必须在同一个记录游标上重做重建
                if swap.is_some() {
                    crate::movepart::fault_after_swap_commit(entry as usize);
                }
                // 起点变了的 NTFS 分区要修 HiddenSectors：数据是字节拷贝，boot sector 还带着旧值。
                // 排在表提交之后、推进游标之前——这一步没做成，重跑就该重试它
                if swap.is_none() {
                    fix_hidden_sectors(src, &reloc, sector_size, sink)?;
                }
                // 换区内容可弃：表项已落位，重建是这一步的后置条件。失败即停止计划——
                // 后续条目要在"本条尚未可用"的布局上继续搬，扩容更不能做
                if let Some(step) = swap {
                    if let Some(item) = swap::rebuild(src, &step, sector_size, sink)? {
                        pending.push(item);
                        return Ok(StepFlow::Stopped);
                    }
                    crate::movepart::fault_after_swap_entry(entry as usize);
                } else {
                    sink.note(&format!(
                        "partition {} relocated (delta {} sectors)",
                        reloc.part,
                        reloc.dst_first.abs_diff(reloc.src_first)
                    ));
                }
                state.entry_index += 1;
                state.chunks_in_entry = 0;
                state.completed_steps += 1;
                sink.table_step_done(src, index, &state)?;
                crate::movepart::fault_after_entry_commit(entry as usize);
                entry += 1;
            }
            PreparedStep::Copy { copy, commit } => {
                let chunk = chunk_bytes
                    .expect("prepare_steps refuses a copy plan without a chunk size");
                copy::execute(src, &copy, sector_size, chunk, state.wrote_target())
                    .map_err(|e| e.context(&format!("plan step {}", index + 1)))?;
                // 复制步骤只在写完数据之后返回 Ok，表步骤此前已写过目标字节
                table::execute(src, commit, true)
                    .map_err(|e| e.context(&format!("plan step {}", index + 1)))?;
                state.completed_steps += 1;
                sink.table_step_done(src, index, &state)?;
            }
            PreparedStep::TableOnly(commit) => {
                // 全部条目搬完之后的那一个表步骤就是扩容：崩溃注入点在这个相位边界上
                if has_transfer && !grow_faulted {
                    crate::movepart::fault_before_grow();
                    grow_faulted = true;
                }
                table::execute(src, commit, state.wrote_target())
                    .map_err(|e| e.context(&format!("plan step {}", index + 1)))?;
                state.completed_steps += 1;
                sink.table_step_done(src, index, &state)?;
            }
            PreparedStep::FsResize(step) => {
                let unmet = fs::execute(src, &step, sector_size, state.wrote_target(), sink)
                    .map_err(|e| e.context(&format!("plan step {}", index + 1)))?;
                state.completed_steps += 1;
                sink.fs_step_done(src, index, &state)?;
                // 文件系统侧没有做成：表已扩、空间尚未可用。后续步骤（若有）不再执行，
                // 记录保留，重跑从这一处继续
                if let Some(item) = unmet {
                    pending.push(item);
                    return Ok(StepFlow::Stopped);
                }
            }
            PreparedStep::FsCreate(step) => {
                // 建立步骤只出现在单步计划里，失败原因直接沿用 fsops 的措辞，不加步骤前缀
                mkfs::execute(src, &step, sector_size, state.wrote_target())?;
                state.completed_steps += 1;
                sink.fs_step_done(src, index, &state)?;
            }
            PreparedStep::FsSet(step) => {
                // 属性步骤同样只出现在单步计划里，屏障在启动外部工具之前落盘
                set::execute(src, &step, state.wrote_target())?;
                state.completed_steps += 1;
                sink.fs_step_done(src, index, &state)?;
            }
        }
    }
    Ok(StepFlow::Complete)
}

/// 执行一份不含后置条件步骤的计划：表步骤、FS 建立与属性步骤都不留未达成的后置条件。
///
/// 待办非空说明装配出了本命令不该有的步骤（今天只有换区重建会产生待办）。按内部错误
/// 报出而不是丢弃——把"后置条件未达成"洗成成功，正是本模块要消灭的那类报告。
pub(crate) fn execute_plan_complete(
    src: &mut WriteSource,
    plan: &ConfirmedPlan,
    chunk_bytes: Option<u64>,
    sink: &mut dyn StepSink,
) -> Result<(), RunOutcome> {
    let mut pending = Vec::new();
    match execute_plan(src, plan, chunk_bytes, sink, &mut pending)? {
        StepFlow::Complete if pending.is_empty() => Ok(()),
        // 待办是"已写盘、还差一步"，按 5 带出明细；报成内部错误会把"盘已改变"洗成
        // "未产生持久化修改"
        _ => Err(RunOutcome::partial_with(
            "a step of this plan left its postcondition unmet",
            pending,
        )),
    }
}

/// 搬移之后按新起点修正 NTFS 的 HiddenSectors。位移为 0 的条目没有可修的东西：
/// 数据没动，位置也没动
fn fix_hidden_sectors(
    src: &mut WriteSource,
    reloc: &Relocate,
    sector_size: u64,
    sink: &mut dyn StepSink,
) -> Result<(), RunOutcome> {
    if reloc.dst_first == reloc.src_first {
        return Ok(());
    }
    let overflow = || {
        RunOutcome::partial(format!(
            "partition {}: the relocated range overflows the address space — the transfer may be incomplete",
            reloc.part
        ))
    };
    let dst_off = reloc.dst_first.checked_mul(sector_size).ok_or_else(overflow)?;
    let total = reloc.sectors.checked_mul(sector_size).ok_or_else(overflow)?;
    let kind = crate::fsid::identify(src, dst_off, total)
        .map_err(|e| RunOutcome::partial(e.to_string()))?;
    if kind != crate::domain::ContentKind::Filesystem(crate::fsid::FsKind::Ntfs) {
        return Ok(());
    }
    crate::movepart::fault_before_hidden_fix();
    let mut emit = |m: &str| sink.note(m);
    crate::movepart::fix_ntfs_hidden_sectors(src, reloc.dst_first, sector_size, &mut emit)
        .map_err(|e| RunOutcome::partial(e.to_string()))
}

/// 首写前比对计划里第一个表步的表区。
///
/// 搬移按计划里的 LBA 直接读写：表在装配取快照之后被外部改动时，搬移会覆盖别人正在
/// 用的区间，等到表步骤执行才发现已经太晚。这里用该步骤自己的 `expect_before` 与
/// `regions` 与盘面现状做字节比较。
///
/// 只看第一个表步：后续表步的前置状态由前一步的写后比对与步前戳链给出。只有 `Relocate`
/// 没有表步的计划已在预扫描的形状校验里拒绝，因此进入本函数的搬移计划必有表步。
fn verify_first_table(src: &WriteSource, steps: &[PreparedStep<'_>]) -> Result<(), RunOutcome> {
    let Some(first) = steps.iter().find_map(|s| match s {
        PreparedStep::Transfer { commit, .. } => Some(*commit),
        PreparedStep::Copy { commit, .. } => Some(*commit),
        PreparedStep::TableOnly(commit) => Some(*commit),
        PreparedStep::FsResize(_) | PreparedStep::FsCreate(_) | PreparedStep::FsSet(_) => None,
    }) else {
        return Ok(());
    };
    let Some(expected) = first.expect_before else {
        return Err(RunOutcome::internal(
            "the first table step carries no expected pre-write stamp — the plan was not assembled; nothing was written",
        ));
    };
    if crate::table::regions_match(src, &first.regions, expected)? {
        return Ok(());
    }
    // 此刻尚未写过任何目标字节，按请求被拒上报
    Err(RunOutcome::refused(
        "the table changed on disk before the transfer started — re-run the command",
    ))
}

/// 首次写入前的全量扫描：类型、装配完整性、表写入范围、计划形状、swap、块参数、
/// 块布局与地址区间，以及记录里的搬移计划与本次计划是否一致。
fn prepare_steps<'a>(
    src: &WriteSource,
    plan: &'a ConfirmedPlan,
    chunk_bytes: Option<u64>,
    sector_size: u64,
    record: Option<&Relocation>,
) -> Result<Vec<PreparedStep<'a>>, RunOutcome> {
    let raw = plan.plan().steps();
    let mut has_data = false;
    for (index, step) in raw.iter().enumerate() {
        match step {
            PlanStep::WriteTable(table) => {
                if table.expect_before.is_none() || table.expect_after.is_none() {
                    return Err(RunOutcome::internal(format!(
                        "plan step {} writes the table but carries no expected stamp — the plan was not assembled; nothing was written",
                        index + 1
                    )));
                }
                // 写前比对要求写入段落在比对区间内且长度一致；放到执行期才判，
                // 前面的条目可能已经写过目标字节
                let aligned = table.writes.iter().all(|w| {
                    w.bytes.len() as u64 == w.region.len && table.regions.contains(&w.region)
                });
                if !aligned {
                    return Err(RunOutcome::internal(format!(
                        "plan step {} writes table bytes outside its comparison regions — nothing was written",
                        index + 1
                    )));
                }
            }
            PlanStep::Relocate { sectors, is_swap, .. } => {
                // 换区条目不搬数据（内容可弃），块参数与块布局都只对数据搬移有意义
                has_data |= !*is_swap;
                if *sectors == 0 {
                    return Err(RunOutcome::internal(format!(
                        "plan step {} relocates zero sectors — nothing was written",
                        index + 1
                    )));
                }
                if !matches!(raw.get(index + 1), Some(PlanStep::WriteTable(_))) {
                    return Err(RunOutcome::internal(format!(
                        "plan step {} relocates data but is not followed by the table step that commits it — nothing was written",
                        index + 1
                    )));
                }
            }
            // 复制步骤与搬移步骤同形：数据在前、提交条目的表步骤紧随其后。载荷自洽与
            // 地址区间都在首写前判定，重复的防御判定在执行期只能按"盘可能已改变"上报
            PlanStep::CopyPartition { part, src_first, dst_first, sectors } => {
                has_data = true;
                if !matches!(raw.get(index + 1), Some(PlanStep::WriteTable(_))) {
                    return Err(RunOutcome::internal(format!(
                        "plan step {} copies data but is not followed by the table step that commits it — nothing was written",
                        index + 1
                    )));
                }
                copy::preflight(
                    src,
                    &CopyPartition {
                        part: *part,
                        src_first: *src_first,
                        dst_first: *dst_first,
                        sectors: *sectors,
                    },
                    sector_size,
                )
                .map_err(|e| e.context(&format!("plan step {}", index + 1)))?;
            }
            // FS 步骤的能力与区间都在首写前判定：缺工具若等到写表之后才暴露，
            // 就只能按"盘可能已改变"上报
            PlanStep::FsResize { part, first_lba, sectors, target_bytes, fstype } => {
                fs::preflight(
                    src,
                    &FsResize {
                        part: *part,
                        first_lba: *first_lba,
                        sectors: *sectors,
                        target_bytes: *target_bytes,
                        fstype: *fstype,
                    },
                    sector_size,
                )
                .map_err(|e| e.context(&format!("plan step {}", index + 1)))?;
            }
            // 建立步骤同理：区间自洽与类型可得都在擦除第一个字节之前判定
            PlanStep::FsCreate { part, first_lba, sectors, fstype, label } => {
                mkfs::preflight(
                    src,
                    &FsCreate {
                        part: *part,
                        first_lba: *first_lba,
                        sectors: *sectors,
                        fstype: *fstype,
                        label: label.clone(),
                    },
                    sector_size,
                )
                .map_err(|e| e.context(&format!("plan step {}", index + 1)))?;
            }
            // 换区重建不在这里判定：它紧跟在自己的表提交之后，能力与区间都由那一步保证。
            // 形状（前后必须是搬移与表提交）由计划的构造点校验
            PlanStep::RebuildSwap { .. } => {}
            // 属性步骤：载荷自洽（分区号、卷标容量、UUID 形态）在写盘之前判定
            PlanStep::FsSet { part, attr, fstype } => {
                set::preflight(&FsSet {
                    part: *part,
                    attr: attr.clone(),
                    fstype: *fstype,
                })
                .map_err(|e| e.context(&format!("plan step {}", index + 1)))?;
            }
        }
    }

    let chunk = if has_data {
        let Some(chunk) = chunk_bytes else {
            return Err(RunOutcome::refused(
                "this plan relocates data but no chunk size was given — nothing was written",
            ));
        };
        const MIN_CHUNK: u64 = 1024 * 1024;
        const MAX_CHUNK: u64 = 64 * 1024 * 1024;
        if !(MIN_CHUNK..=MAX_CHUNK).contains(&chunk) || chunk % sector_size != 0 {
            return Err(RunOutcome::refused(format!(
                "chunk size {chunk} bytes must be within 1 MiB..=64 MiB and a multiple of the {sector_size}-byte sector — nothing was written"
            )));
        }
        Some(chunk)
    } else {
        None
    };

    // 记录与本次计划的搬移形状必须一致：按别人的条目数或块大小续跑会把块进度落到错误的位置
    if let Some(reloc) = record {
        let planned = raw
            .iter()
            .filter(|s| matches!(s, PlanStep::Relocate { .. }))
            .count();
        let chunk_matches = match chunk {
            Some(chunk) => chunk == reloc.chunk_bytes,
            None => true,
        };
        if reloc.entries.len() != planned || !chunk_matches {
            return Err(RunOutcome::partial(format!(
                "the recovery record holds {} relocation entries of {}-byte chunks, this plan holds {planned} — the transfer may be incomplete",
                reloc.entries.len(),
                reloc.chunk_bytes
            )));
        }
        // 布局等式的块数比较要求两处扇区大小相同：同一 LBA 长度在不同扇区下换算出不同的块数
        if reloc.ss != sector_size {
            return Err(RunOutcome::partial(format!(
                "the recovery record was written with a {}-byte sector, this target has {sector_size} — the transfer may be incomplete",
                reloc.ss
            )));
        }
    }

    if let Some(chunk) = chunk {
        let mut reloc_index = 0u32;
        for (index, step) in raw.iter().enumerate() {
            let PlanStep::Relocate { part, src_first, dst_first, sectors, is_swap } = step else {
                continue;
            };
            // 换区不搬数据：它没有块布局，块的进度恒为 0（记录里也是 0）
            if *is_swap {
                reloc_index += 1;
                continue;
            }
            let shift = dst_first.abs_diff(*src_first);
            let Some((_, count)) =
                move_chunk_layout(*sectors, sector_size, shift, *sectors, chunk)
            else {
                return Err(RunOutcome::internal(format!(
                    "plan step {} has no usable relocation block layout — nothing was written",
                    index + 1
                )));
            };
            // 地址区间的溢出判定与布局同源：执行期再遇到时前面的条目可能已写过
            let src_base = src_first.checked_mul(sector_size).ok_or_else(|| {
                RunOutcome::internal(format!(
                    "plan step {}: the source offset of partition {part} overflows the address space — nothing was written",
                    index + 1
                ))
            })?;
            let dst_base = dst_first.checked_mul(sector_size).ok_or_else(|| {
                RunOutcome::internal(format!(
                    "plan step {}: the target offset of partition {part} overflows the address space — nothing was written",
                    index + 1
                ))
            })?;
            let total = sectors.checked_mul(sector_size).ok_or_else(|| {
                RunOutcome::internal(format!(
                    "plan step {}: the relocation length of partition {part} overflows the address space — nothing was written",
                    index + 1
                ))
            })?;
            if src_base.checked_add(total).is_none() || dst_base.checked_add(total).is_none() {
                return Err(RunOutcome::internal(format!(
                    "plan step {}: the relocation byte range of partition {part} overflows the address space — nothing was written",
                    index + 1
                )));
            }
            // 记录里当前条目已完成的块数不能超过该条目的块数
            if let Some(reloc) = record
                && reloc.entry_index == reloc_index
                && reloc.chunks_in_entry > count
            {
                return Err(RunOutcome::internal(format!(
                    "plan step {}: {} chunks are recorded as done but the step only has {count} — nothing was written",
                    index + 1,
                    reloc.chunks_in_entry
                )));
            }
            reloc_index += 1;
        }
    }

    let mut steps = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        match &raw[index] {
            PlanStep::Relocate { part, src_first, dst_first, sectors, is_swap } => {
                // 形状校验在装配层，但计划是外部输入：越界必须是错误而不是 panic
                let Some(PlanStep::WriteTable(commit)) = raw.get(index + 1) else {
                    return Err(RunOutcome::internal(
                        "the relocation step is not followed by a table step — nothing was written",
                    ));
                };
                // 换区条目在表提交之后多一步重建；两处对"这是不是换区"必须同答，
                // 否则表会落位而内容不重建（或反之）
                let swap = match raw.get(index + 2) {
                    Some(PlanStep::RebuildSwap { part: p, src_first: s0, dst_first: d0, sectors: n }) => {
                        Some(SwapRebuild { part: *p, src_first: *s0, dst_first: *d0, sectors: *n })
                    }
                    _ => None,
                };
                if swap.is_some() != *is_swap {
                    return Err(RunOutcome::internal(format!(
                        "plan step {}: the swap-rebuild step and the relocation entry disagree on the partition type — nothing was written",
                        index + 1
                    )));
                }
                index += 2 + usize::from(swap.is_some());
                steps.push(PreparedStep::Transfer {
                    reloc: Relocate {
                        part: *part,
                        src_first: *src_first,
                        dst_first: *dst_first,
                        sectors: *sectors,
                    },
                    commit,
                    swap,
                });
            }
            PlanStep::RebuildSwap { .. } => {
                return Err(RunOutcome::internal(
                    "a swap-rebuild step is not attached to the relocation entry that owns it — nothing was written",
                ));
            }
            PlanStep::CopyPartition { part, src_first, dst_first, sectors } => {
                let Some(PlanStep::WriteTable(commit)) = raw.get(index + 1) else {
                    return Err(RunOutcome::internal(
                        "the copy step is not followed by a table step — nothing was written",
                    ));
                };
                steps.push(PreparedStep::Copy {
                    copy: CopyPartition {
                        part: *part,
                        src_first: *src_first,
                        dst_first: *dst_first,
                        sectors: *sectors,
                    },
                    commit,
                });
                index += 2;
            }
            PlanStep::WriteTable(commit) => {
                steps.push(PreparedStep::TableOnly(commit));
                index += 1;
            }
            PlanStep::FsResize { part, first_lba, sectors, target_bytes, fstype } => {
                steps.push(PreparedStep::FsResize(FsResize {
                    part: *part,
                    first_lba: *first_lba,
                    sectors: *sectors,
                    target_bytes: *target_bytes,
                    fstype: *fstype,
                }));
                index += 1;
            }
            PlanStep::FsCreate { part, first_lba, sectors, fstype, label } => {
                steps.push(PreparedStep::FsCreate(FsCreate {
                    part: *part,
                    first_lba: *first_lba,
                    sectors: *sectors,
                    fstype: *fstype,
                    label: label.clone(),
                }));
                index += 1;
            }
            PlanStep::FsSet { part, attr, fstype } => {
                steps.push(PreparedStep::FsSet(FsSet {
                    part: *part,
                    attr: attr.clone(),
                    fstype: *fstype,
                }));
                index += 1;
            }
        }
    }
    Ok(steps)
}

/// 执行状态从记录纳入：游标跨运行累计，续跑必须从已落盘的值继续。
///
/// 记录里存在搬移计划时，全局块游标由计划重算：已提交条目的块数之和加当前条目已完成的
/// 块数，与 `progress.transfer_cursor` 取较大者。
fn journal_state(src: &WriteSource) -> Result<TransferState, RunOutcome> {
    let Some(journal) = src.record_journal() else {
        return Ok(TransferState::default());
    };
    let progress = journal.progress();
    let mut state = TransferState {
        entry_index: 0,
        chunks_in_entry: 0,
        completed_steps: progress.completed_steps,
        transfer_cursor: progress.transfer_cursor,
    };
    let Some(reloc) = journal.relocation() else {
        return Ok(state);
    };
    state.entry_index = reloc.entry_index;
    state.chunks_in_entry = reloc.chunks_in_entry;
    let mut recomputed = 0u64;
    for entry in reloc.entries.iter().take(reloc.entry_index as usize) {
        let count = recorded_entry_chunks(entry, reloc)?;
        recomputed = recomputed.checked_add(count).ok_or_else(|| {
            RunOutcome::internal(
                "the recorded move cursor overflows the address space — nothing was written",
            )
        })?;
    }
    recomputed = recomputed.checked_add(reloc.chunks_in_entry).ok_or_else(|| {
        RunOutcome::internal(
            "the recorded move cursor overflows the address space — nothing was written",
        )
    })?;
    state.transfer_cursor = state.transfer_cursor.max(recomputed);
    Ok(state)
}

/// 记录里一条搬移已完成的块数。位移为 0 与 swap 条目不搬数据，块数为 0。
fn recorded_entry_chunks(entry: &RelocEntry, reloc: &Relocation) -> Result<u64, RunOutcome> {
    if entry.delta == 0 || entry.is_swap {
        return Ok(0);
    }
    move_chunk_layout(entry.len, reloc.ss, entry.delta, entry.len, reloc.chunk_bytes)
        .map(|(_, count)| count)
        .ok_or_else(|| {
            RunOutcome::partial(format!(
                "the recovery record's relocation entry for partition {} has no usable block layout — the transfer may be incomplete",
                entry.partition
            ))
        })
}

/// 恢复记录的默认块字节数：8 MiB。
///
/// 依据：`Transfer` 每前进 `max(ChunkSize, 8 MiB)` 替换一次记录并只
/// 更新 `cursor`，而 `resize-part` 不登记 `--chunk`，恢复粒度取该下限
pub(crate) const DEFAULT_CHUNK_BYTES: u64 = 8 * 1024 * 1024;
