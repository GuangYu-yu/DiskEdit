//! 回滚的签发与执行。
//!
//! 准入核对（记录资格、身份、区间形状、源复核、盘面比对）全部通过后才产出
//! [`ProvisionalRecord`] 与 [`RollbackPermit`]；[`execute_rollback`] 按值消费两者，
//! 把记录里的原字节逆序写回并释放记录。两个凭据都不可克隆：构造字段私有，
//! crate 内除签发点外拿不到，回放的每一步写入因此都处在同一套核对之下。

use super::{OpKind, Record, Relocation, RestoreEntry, Stage, settle_companion};
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
/// 全部核对通过后签发。`scope` 决定回放结果怎么向用户交待
pub(crate) struct RollbackPermit {
    scope: TableReplay,
    _seal: Seal,
}

/// 签发点的封印：字段私有，其它模块构造不了带它的值
struct Seal;

/// 记录越过不可逆点后，`undo` 能写回哪些区间。判据是"恢复后表所指的旧区间是否已被
/// 已写的目的区间压过"：`copy` 不写源区，旧表指向的数据仍完整；`move` 把数据搬到新位置，
/// 只有已写目的区与旧区间不相交时，旧表才仍然指向完整数据。
///
/// 旧区间取自记录里 `restore` 的预映像字节（写表之前的旧表）——当前盘面已是新布局，
/// 不能用它反推旧布局。记录里没有可枚举的旧表时拒绝，并说明是没有表快照，
/// 不把它说成"已写区间相交"。
///
/// 逐条目比较位移会把尾打包看漏：后一条的目的区可以复写前一条的旧位。所以判据取
/// 全部已写目的区间与旧表全部分区区间的交集，而不是"位移 ≥ 长度"。
///
/// `undo` 的准入、`inspect` 的下一步提示、`abandon` 的放弃计数与"目标忙"的出路文案
/// 共用这一份判据：各处各写一遍匹配式，就会出现"提示说能撤、`undo` 说不能"的漂移
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TableReplay {
    /// 记录未越过不可逆点：记录里的区间按原样回放
    Full,
    /// `copy` 越过不可逆点：只回放表区，源区数据未被触碰
    CopyTableOnly,
    /// `move` 越过不可逆点，但已写目的区不碰旧区间：可以回放表区
    MoveClear,
    /// `move` 越过不可逆点且已写目的区压到旧区间（或记录不足以证明没压到）：拒绝
    MoveBlocked,
    /// `move` 越过不可逆点，但记录里没有可回放的旧表快照（未记录或解析不出）：拒绝，
    /// 显式许可也不放行——没有可回放的东西，放行就是空操作假成功
    MoveNoPreimage,
    /// 同上，但调用方给了显式许可：只回放表区，旧布局指名的数据已不可信
    MoveForced,
    /// 其它命令的数据写入越过不可逆点：记录的区间不回放
    Refused,
}

impl TableReplay {
    /// 判据：`permit_move` 是命令行的显式许可，只对 `move` 有效
    pub(crate) fn of(record: &Record, permit_move: bool) -> Self {
        if !record.restore_blocked {
            return Self::Full;
        }
        match record.op {
            OpKind::Copy => Self::CopyTableOnly,
            OpKind::Move => match (move_writes_overlap_old_ranges(record), permit_move) {
                (None, _) => Self::MoveNoPreimage,
                (Some(false), _) => Self::MoveClear,
                (Some(true), true) => Self::MoveForced,
                (Some(true), false) => Self::MoveBlocked,
            },
            _ => Self::Refused,
        }
    }

    /// 记录里的区间是否还会被写回
    pub(crate) fn writes_back(self) -> bool {
        !matches!(self, Self::MoveBlocked | Self::MoveNoPreimage | Self::Refused)
    }
}

impl std::fmt::Display for TableReplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("the recorded ranges are replayed in full"),
            Self::CopyTableOnly => f.write_str(
                "the recorded `copy` did not write the ranges its table names, so the old layout's data \
                 remains intact",
            ),
            Self::MoveClear => f.write_str(
                "the partition table is back to the old layout, but the recorded ranges were not rolled back; \
                 no destination range the move has written, or may have written, overlaps a range that layout \
                 names (verify with: diskedit info)",
            ),
            Self::MoveBlocked => f.write_str(
                "the ranges the old layout names cannot be shown to be clear of the ranges the move has \
                 already written, so its bytes are not written back; re-run `diskedit move` with the same \
                 request to finish the relocation, or release the record with `diskedit abandon`",
            ),
            Self::MoveNoPreimage => f.write_str(
                "the record holds no replayable copy of the old partition table — the table step never ran, \
                 or the recorded bytes do not parse — so no table bytes are written back; re-run \
                 `diskedit move` with the same request to finish the relocation, or release the record \
                 with `diskedit abandon`",
            ),
            Self::MoveForced => f.write_str(
                "the partition table is back to the old layout, but the recorded ranges were not rolled back; \
                 at least one destination range the move has written, or may have written, overlaps a range \
                 that layout names, so the data they hold is no longer trustworthy (verify with: diskedit info)",
            ),
            Self::Refused => f.write_str(
                "the record marks a non-reversible mutation, so the recorded bytes are not written back; \
                 release the transaction with `diskedit abandon`",
            ),
        }
    }
}

/// 已写的目的区间是否压到恢复后表所指的旧区间。`None` = 记录里没有可枚举的旧表快照，
/// 调用方据此把"没有东西可回放"与"实际相交"分开陈述。
///
/// 已写范围 = 游标之前的条目（数据已拷完，目的区整段已写）加上游标条目整段目的区：
/// 块写入与游标推进是两次落盘，崩溃可以停在"块已写、游标未推进"之间，记录里的
/// `chunks_in_entry` 只是已写下界。swap 条目不拷数据，但表项提交后按新落点 mkswap
/// 重写该区，目的区同样整段按已写计。旧范围 = 记录里 `restore` 预映像那张表所指的
/// 全部分区。记录自相矛盾（位移算不出、块布局与执行器对不上、游标越界）时按相交
/// 处理，不把"证明不了没压到"当成"安全"。
fn move_writes_overlap_old_ranges(record: &Record) -> Option<bool> {
    let Some(reloc) = record.relocation.as_ref() else {
        return Some(true);
    };
    let old = restore_preimage_ranges(&record.restore)?;
    let Some(written) = written_destination_ranges(reloc) else {
        return Some(true);
    };
    Some(ranges_intersect(&written, &old))
}

/// 记录已声明的已写目的区间（单位 LBA）。`None` 表示记录自相矛盾，调用方按相交处理
fn written_destination_ranges(reloc: &Relocation) -> Option<Vec<(u64, u64)>> {
    let cursor = reloc.entry_index as usize;
    if cursor > reloc.entries.len() {
        return None;
    }
    let mut written = Vec::new();
    for entry in reloc.entries.iter().take(cursor) {
        // 零位移条目既没拷数据也没换落点；swap 条目虽有位移不拷数据，但 mkswap 会
        // 按新落点重写目的区，不能跳过
        if entry.delta == 0 && !entry.is_swap {
            continue;
        }
        written.push((entry.src_first.checked_add_signed(entry.delta)?, entry.len));
    }
    if let Some(entry) = reloc.entries.get(cursor) {
        if entry.is_swap {
            // 表项可能已提交、mkswap 可能已在新落点写过，目的区整段按已写计
            if reloc.chunks_in_entry != 0 {
                return None;
            }
            written.push((entry.src_first.checked_add_signed(entry.delta)?, entry.len));
        } else if entry.delta == 0 {
            // 不拷贝的条目不可能有进度
            if reloc.chunks_in_entry != 0 {
                return None;
            }
        } else {
            let (chunk, count) = crate::planner::chunk::move_chunk_layout(
                entry.len,
                reloc.ss,
                entry.delta.unsigned_abs(),
                entry.len,
                reloc.chunk_bytes,
            )?;
            let chunk_lba = chunk / reloc.ss;
            let spans = crate::planner::chunk::spans(entry.src_first, entry.len, chunk_lba).count() as u64;
            if chunk % reloc.ss != 0 || spans != count {
                return None;
            }
            // 块写入与游标推进是两次落盘：游标没推进的条目也可能已经写过若干块，
            // 而盘上推不出是哪些块。整段目的区都按已写计，多算只会让结论偏向拒绝
            written.push((entry.src_first.checked_add_signed(entry.delta)?, entry.len));
        }
    }
    Some(written)
}

/// 记录里 `restore` 预映像所构成的那张表所指的分区区间（单位 LBA）。
///
/// 预映像字节是写表之前的旧表；当前盘面已是新布局，不参与判定。返回 `None`
/// 表示预映像不构成一张能完整枚举分区区间的表——调用方单列为"没有可回放的旧表"，
/// 与"已写区间相交"分开陈述。
/// 取不出区间的原因包括：缺表头/条目数组的段、头或数组 CRC 不符、保护性 MBR、
/// 扩展分区链（预映像里没有 EBR）、字段自相矛盾、一张表却没有任何分区
fn restore_preimage_ranges(restore: &[RestoreEntry]) -> Option<Vec<(u64, u64)>> {
    if let Some(primary) = restore_preimage(restore, 1)
        && primary.len() >= crate::table::GPT_SIGNATURE.len()
        && &primary[..crate::table::GPT_SIGNATURE.len()] == crate::table::GPT_SIGNATURE
    {
        return gpt_preimage_ranges(restore, primary);
    }
    mbr_preimage_ranges(restore)
}

/// 预映像里从 `first_lba` 起的那一段字节
fn restore_preimage(restore: &[RestoreEntry], first_lba: u64) -> Option<&[u8]> {
    restore.iter().find(|e| e.first_lba == first_lba).map(|e| e.original.as_slice())
}

/// GPT 旧布局：主头优先；主头不可读时用盘尾备份头。条目数组从预映像拼出并校验 CRC
fn gpt_preimage_ranges(restore: &[RestoreEntry], primary: &[u8]) -> Option<Vec<(u64, u64)>> {
    // 规范固定头字段共 92 字节：偏移 12 的 header_size 与偏移 88 的条目数组 CRC 都在其中。
    // 短于此的段构不成可用的头，读这两处固定偏移会越界
    if primary.len() < 92 {
        return None;
    }
    let header = match crate::table::probe_header(primary) {
        crate::table::HeaderProbe::Present(h) if h.primary_lba == 1 => h,
        _ => backup_preimage_header(restore)?.0,
    };
    let array_len = (header.number_of_partition_entries as usize)
        .checked_mul(header.size_of_partition_entry as usize)?;
    // 条目结构是规范固定的 128 字节；短于此的步长不属于 GPT 条目数组
    if header.size_of_partition_entry < 128 || header.number_of_partition_entries == 0 {
        return None;
    }
    let array = restore_preimage(restore, header.partition_entry_lba)?;
    if array.len() < array_len {
        return None;
    }
    let array = &array[..array_len];
    if crate::table::rd_u32(primary, 88) != crate::table::crc32(array) {
        return None;
    }
    let mut out = Vec::new();
    for entry in crate::table::parse_entry_array(
        array,
        header.number_of_partition_entries as usize,
        header.size_of_partition_entry as usize,
    ) {
        if entry.starting_lba == 0 && entry.ending_lba == 0 {
            continue;
        }
        if entry.starting_lba > entry.ending_lba {
            return None;
        }
        out.push((entry.starting_lba, entry.ending_lba - entry.starting_lba + 1));
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// 预映像里的备份 GPT 头：MyLBA 是本段起点、AlternateLBA 指回 LBA1
fn backup_preimage_header(restore: &[RestoreEntry]) -> Option<(crate::table::RawHeader, u64)> {
    for entry in restore {
        // 短于规范最小头长的段（MBR 签名点）不可能是 GPT 头
        if entry.original.len() < 92 {
            continue;
        }
        if let crate::table::HeaderProbe::Present(h) = crate::table::probe_header(&entry.original)
            && h.primary_lba == entry.first_lba
            && h.backup_lba == 1
        {
            return Some((h, entry.first_lba));
        }
    }
    None
}

/// MBR 旧布局：LBA0 预映像的四个主条目。保护性 MBR 与扩展分区链都列不出完整数据区间
fn mbr_preimage_ranges(restore: &[RestoreEntry]) -> Option<Vec<(u64, u64)>> {
    let sector = restore_preimage(restore, 0)?;
    if sector.len() < 512 || sector[510..512] != [0x55, 0xAA] {
        return None;
    }
    let mut out = Vec::new();
    for slot in 0..4 {
        let entry = &sector[446 + slot * 16..446 + (slot + 1) * 16];
        let kind = entry[4];
        if kind == 0 {
            continue;
        }
        // 0xEE 是保护 MBR（这张表其实是 GPT）；0x05/0x0F/0x85 指向 EBR 链，
        // 逻辑分区只在链上，LBA0 这段字节枚举不出来
        if kind == 0xEE || matches!(kind, 0x05 | 0x0F | 0x85) {
            return None;
        }
        let first = u64::from(crate::table::rd_u32(entry, 8));
        let sectors = u64::from(crate::table::rd_u32(entry, 12));
        if sectors == 0 {
            return None;
        }
        out.push((first, sectors));
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// 两组 `(起点, 长度)` 的交集；任一端点溢出按相交处理
fn ranges_intersect(a: &[(u64, u64)], b: &[(u64, u64)]) -> bool {
    a.iter().any(|&(a_first, a_len)| {
        b.iter().any(|&(b_first, b_len)| {
            match (a_first.checked_add(a_len), b_first.checked_add(b_len)) {
                (Some(a_end), Some(b_end)) => a_first < b_end && b_first < a_end,
                _ => true,
            }
        })
    })
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
        permit_move: bool,
    ) -> Admission {
        // `move` 的表区 preimage 只在"已写目的区没压到旧区间"时还能安全回写；`copy`
        // 不写源区，表区回放始终安全。判据落在 [`TableReplay`] 一处，这里只做 match
        let scope = TableReplay::of(&record, permit_move);
        if !scope.writes_back() {
            bail_fail(RunOutcome::refused(format!(
                "the recovery record at {}: {scope}",
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
        Admission::Proceed(
            ProvisionalRecord { record, path: path.to_path_buf() },
            RollbackPermit { scope, _seal: Seal },
        )
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
    let RollbackPermit { scope, .. } = permit;
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
        match scope {
            // `move` 的表区回放不能让中性措辞盖过"旧区间数据不可信"这一事实
            TableReplay::MoveClear | TableReplay::MoveForced => scope.to_string(),
            _ => format!("undid {n} recorded write range(s) (verify with: diskedit info)"),
        }
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
            return Err(RunOutcome::refused(format!(
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
        bail_fail(RunOutcome::refused(format!(
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
        RunOutcome::refused(format!(
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
    use super::*;
    use crate::dev::DiskGuidState;
    use crate::platform::{IdentityMaterial, TargetId};
    use crate::recovery::{PlanDigest, PlanId, PlanVersion, RelocEntry, RelocKind, SceneIdentity, TxId};

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

    fn preimage_with_sector(first_lba: u64, sector_size: u64, bytes: Vec<u8>) -> RestoreEntry {
        RestoreEntry {
            sector_size: sector_size as u32,
            first_lba,
            sectors: (bytes.len() as u64 / sector_size) as u32,
            original: bytes.clone(),
            after: bytes,
        }
    }

    fn preimage(first_lba: u64, bytes: Vec<u8>) -> RestoreEntry {
        preimage_with_sector(first_lba, 512, bytes)
    }

    /// 一张一分区 GPT 的预映像：LBA1 主头 + LBA2 条目数组
    fn gpt_preimages(start: u64, end: u64) -> Vec<RestoreEntry> {
        gpt_preimages_sized(512, 128, 4, start, end)
    }

    /// 按给定扇区大小与条目步长构造预映像：数组占一整段预映像，条目数与步长写进
    /// 头里，CRC 覆盖整段
    fn gpt_preimages_sized(
        sector_size: u64,
        entry_size: u32,
        entries: u32,
        start: u64,
        end: u64,
    ) -> Vec<RestoreEntry> {
        let mut array = vec![0u8; entry_size as usize * entries as usize];
        array[0] = 0x01; // 非零类型 GUID：条目已用
        array[32..40].copy_from_slice(&start.to_le_bytes());
        array[40..48].copy_from_slice(&end.to_le_bytes());
        let header = gpt_header_for(sector_size, entry_size, entries, 1, 100, crate::table::crc32(&array));
        vec![
            preimage_with_sector(1, sector_size, header),
            preimage_with_sector(2, sector_size, array),
        ]
    }

    /// 一个 512 字节的 GPT 头：`primary_lba`/`backup_lba` 与条目数组 CRC 由图给出
    fn gpt_preimage_header(primary_lba: u64, backup_lba: u64, array_crc: u32) -> Vec<u8> {
        gpt_header_for(512, 128, 4, primary_lba, backup_lba, array_crc)
    }

    fn gpt_header_for(
        sector_size: u64,
        entry_size: u32,
        entries: u32,
        primary_lba: u64,
        backup_lba: u64,
        array_crc: u32,
    ) -> Vec<u8> {
        let mut header = vec![0u8; sector_size as usize];
        header[0..8].copy_from_slice(crate::table::GPT_SIGNATURE);
        header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        header[12..16].copy_from_slice(&92u32.to_le_bytes());
        header[24..32].copy_from_slice(&primary_lba.to_le_bytes());
        header[32..40].copy_from_slice(&backup_lba.to_le_bytes());
        header[40..48].copy_from_slice(&34u64.to_le_bytes());
        header[48..56].copy_from_slice(&99u64.to_le_bytes());
        header[56..72].copy_from_slice(&[0x5A; 16]);
        header[72..80].copy_from_slice(&2u64.to_le_bytes());
        header[80..84].copy_from_slice(&entries.to_le_bytes());
        header[84..88].copy_from_slice(&entry_size.to_le_bytes());
        header[88..92].copy_from_slice(&array_crc.to_le_bytes());
        let crc = crate::table::crc32(&header[..92]);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
        header
    }

    /// 一张一分区 MBR 的预映像
    fn mbr_preimage(kind: u8) -> Vec<RestoreEntry> {
        let mut sector = vec![0u8; 512];
        sector[446 + 4] = kind;
        sector[446 + 8..446 + 12].copy_from_slice(&2048u32.to_le_bytes());
        sector[446 + 12..446 + 16].copy_from_slice(&2048u32.to_le_bytes());
        sector[510] = 0x55;
        sector[511] = 0xAA;
        vec![preimage(0, sector)]
    }

    #[test]
    fn old_ranges_come_from_the_restore_preimage() {
        assert_eq!(restore_preimage_ranges(&gpt_preimages(2048, 4095)), Some(vec![(2048, 2048)]));
        assert_eq!(restore_preimage_ranges(&mbr_preimage(0x83)), Some(vec![(2048, 2048)]));
    }

    /// 预映像读不成一张能枚举区间的表时一律按相交处理，不把「证明不了」当成「安全」
    #[test]
    fn an_unreadable_preimage_counts_as_an_overlap() {
        let mut missing_array = gpt_preimages(2048, 4095);
        missing_array.truncate(1);
        assert_eq!(restore_preimage_ranges(&missing_array), None, "a missing array section is not empty");

        let mut bad_crc = gpt_preimages(2048, 4095);
        bad_crc[1].original[0] ^= 0x01;
        assert_eq!(restore_preimage_ranges(&bad_crc), None, "an array whose CRC does not match is unreadable");

        assert_eq!(restore_preimage_ranges(&mbr_preimage(0xEE)), None, "a protective MBR is a GPT marker");
        assert_eq!(restore_preimage_ranges(&mbr_preimage(0x05)), None, "an extended chain is not enumerable");
        assert_eq!(restore_preimage_ranges(&[]), None, "no preimage at all");

        let written = vec![(4096u64, 2048u64)];
        assert!(
            !ranges_intersect(&written, &[(2048, 2048)]),
            "ranges that only touch at an endpoint do not overlap"
        );
    }

    /// 一条右移的搬移记录：游标停在 `entry_index`，该条目还没记下任何块进度
    fn move_record(entries: Vec<RelocEntry>, entry_index: u32, old: (u64, u64)) -> Record {
        move_record_with(entries, entry_index, 512, gpt_preimages(old.0, old.0 + old.1 - 1))
    }

    fn move_record_with(
        entries: Vec<RelocEntry>,
        entry_index: u32,
        ss: u64,
        restore: Vec<RestoreEntry>,
    ) -> Record {
        let mut record = Record::new(
            TxId::from_bytes([1; 16]),
            SceneIdentity::from_target(
                TargetId::from_material(IdentityMaterial::image(Some(7), Some([3; 16])).unwrap()),
            ),
            PlanId::from_bytes([2; 16]),
            PlanVersion::new(1),
            OpKind::Move,
            PlanDigest::from_bytes([4; 32]),
            DiskGuidState::Unavailable,
        );
        record.restore_blocked = true;
        record.relocation = Some(Relocation {
            entries,
            entry_index,
            chunks_in_entry: 0,
            chunk_bytes: 1 << 20,
            kind: RelocKind::Shift,
            grow_part: 0,
            disk_size: 8 * 1024 * 1024,
            ss,
        });
        record.restore = restore;
        record
    }

    fn move_entry(src_first: u64, len: u64, delta: i64) -> RelocEntry {
        RelocEntry { partition: 1, src_first, len, delta, is_swap: false }
    }

    fn swap_entry(src_first: u64, len: u64, delta: i64) -> RelocEntry {
        RelocEntry { partition: 2, src_first, len, delta, is_swap: true }
    }

    /// 块写入与游标推进是两次落盘：游标没推进的条目也可能已经写过若干块，盘上又推不出
    /// 是哪些块，故当前条目的整段目的区都按已写计。压到旧区间必须拒绝，整段都不碰才放行
    #[test]
    fn a_lagging_cursor_counts_the_whole_current_entry_as_written() {
        // 源 2048..6144，右移 2048：目的 4096..8192 压在旧区间 2048..6144 上
        let overlapping = move_record(vec![move_entry(2048, 4096, 2048)], 0, (2048, 4096));
        assert_eq!(TableReplay::of(&overlapping, false), TableReplay::MoveBlocked);
        assert!(!TableReplay::of(&overlapping, false).writes_back());

        // 右移整段长度：目的 6144..10240 与旧区间只在端点相接
        let clear = move_record(vec![move_entry(2048, 4096, 4096)], 0, (2048, 4096));
        assert_eq!(TableReplay::of(&clear, false), TableReplay::MoveClear);
        assert!(TableReplay::of(&clear, false).writes_back());

        // 文案不越过证据：只说"已写目的区与旧区间不相交"，不说"没往那些区间写过"
        let clear_text = TableReplay::of(&clear, false).to_string();
        assert!(clear_text.contains("no destination range the move has written"), "{clear_text}");
        assert!(!clear_text.contains("wrote no data"), "{clear_text}");

        // 显式许可下同样只声明相交，不声明把旧区间整段都写过
        let forced = TableReplay::of(&overlapping, true);
        assert_eq!(forced, TableReplay::MoveForced);
        let forced_text = forced.to_string();
        assert!(
            forced_text.contains("at least one destination range the move has written"),
            "{forced_text}"
        );
        assert!(!forced_text.contains("has written the ranges"), "{forced_text}");
    }

    /// swap 条目不拷数据，但表项提交后 `swap::rebuild` 按新落点重写该区：目的区整段按
    /// 已写计。目的区压到旧区间时只能更严——游标停在该条目上（表项可能已提交、mkswap
    /// 可能已跑过）与游标已越过它都一样拒绝
    #[test]
    fn a_swap_destination_counts_as_written() {
        // 源 8192..12288 左移 6144：目的 2048..6144 就是旧布局命名的区间
        for cursor in [0, 1] {
            let blocked = move_record(vec![swap_entry(8192, 4096, -6144)], cursor, (2048, 4096));
            assert_eq!(TableReplay::of(&blocked, false), TableReplay::MoveBlocked, "cursor {cursor}");
            assert_eq!(TableReplay::of(&blocked, true), TableReplay::MoveForced, "cursor {cursor}");
        }
        // 右移整段长度：目的 14336..18432 与旧区间不相交，仍可回放
        let clear = move_record(vec![swap_entry(8192, 4096, 6144)], 1, (2048, 4096));
        assert_eq!(TableReplay::of(&clear, false), TableReplay::MoveClear);
    }

    /// 记录里没有旧表快照（表步没跑过）或快照解析不出时，拒绝原因说的是"没有可回放的
    /// 旧表"；显式许可也不放行——没有东西可回放时放行就是空操作假成功
    #[test]
    fn a_move_without_a_table_snapshot_refuses_even_when_forced() {
        let empty = move_record_with(vec![move_entry(2048, 4096, 4096)], 0, 512, Vec::new());
        let garbled =
            move_record_with(vec![move_entry(2048, 4096, 4096)], 0, 512, vec![preimage(1, vec![0u8; 512])]);
        for record in [&empty, &garbled] {
            for permit in [false, true] {
                assert_eq!(TableReplay::of(record, permit), TableReplay::MoveNoPreimage);
                assert!(!TableReplay::of(record, permit).writes_back());
            }
        }
        assert!(TableReplay::of(&empty, false).to_string().contains("no replayable copy"));
        assert!(!TableReplay::of(&empty, false).to_string().contains("has already written"));
    }

    /// 4K 扇区的预映像同样读得出旧布局：头与条目数组各按一个 4096 字节扇区落段，
    /// 解析不因默认的 512 落空；不相交的搬移仍判为可回放
    #[test]
    fn a_4k_preimage_reads_and_keeps_the_move_clear() {
        let restore = gpt_preimages_sized(4096, 128, 32, 2048, 6143);
        assert_eq!(restore_preimage_ranges(&restore), Some(vec![(2048, 4096)]));
        let record = move_record_with(vec![move_entry(2048, 4096, 4096)], 0, 4096, restore);
        assert_eq!(TableReplay::of(&record, false), TableReplay::MoveClear);
    }

    /// 条目步长 256 的 GPT：CRC 覆盖整段数组，区间按步长枚举，结论不因步长而变
    #[test]
    fn an_entry_size_of_256_reads_as_a_usable_preimage() {
        let restore = gpt_preimages_sized(512, 256, 2, 2048, 6143);
        assert_eq!(restore_preimage_ranges(&restore), Some(vec![(2048, 4096)]));
        let record = move_record_with(vec![move_entry(2048, 4096, 4096)], 0, 512, restore);
        assert_eq!(TableReplay::of(&record, false), TableReplay::MoveClear);
    }

    /// 短段构不成可用的 GPT 头：固定偏移读不出来时返回 None（按相交处理），不越界
    #[test]
    fn a_short_gpt_preimage_is_unreadable_not_a_panic() {
        // 签名之后不足 16 字节：header_size 读不出来
        let mut truncated = crate::table::GPT_SIGNATURE.to_vec();
        truncated.resize(12, 0);
        assert_eq!(restore_preimage_ranges(&[preimage(1, truncated)]), None);

        // 16..92：header_size 可读但固定字段不满；备份头在场时也不得回头读越界
        let mut primary = crate::table::GPT_SIGNATURE.to_vec();
        primary.resize(91, 0);
        primary[12..16].copy_from_slice(&92u32.to_le_bytes());
        let backup = gpt_preimage_header(8, 1, 0);
        assert_eq!(restore_preimage_ranges(&[preimage(1, primary), preimage(8, backup)]), None);
    }
}
