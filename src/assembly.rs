//! 计划期装配：读取已打开的目标，把表步骤装配到可执行形态。
//!
//! 本模块处在组合位置，是唯一同时看见目标句柄与计划值类型的地方。它只读：不建记录、
//! 不建临时文件、不取写锁、不写任何字节。计划值本身不含 I/O。
//!
//! 表步骤只有一种可执行形态：经 [`assemble_table_step`] 得到的
//! [`AssembledTableStep`]。它的字段私有且唯一构造点要求 `&FileSource`，所以"已装配"
//! 由类型保证，不靠调用方约定。

use crate::dev::FileSource;
use crate::domain::{ContentKind, SectorSize};
use crate::geometry::ValidatedGeometry;
use crate::gpt_policy::GptFlag;
use crate::movepart::{is_forbidden_type_guid, is_swap_guid};
use crate::outcome::RunOutcome;
use crate::planner::{
    ConfirmedPlan, FsSetAttr, PartitionFact, PlanStep, TableFacts, TableObservation, UuidValue,
};
use crate::table::{
    GptContent, GptError, MbrPartition, RawGpt, TableLabel, TableSnapshot, TableStamp,
    TableStep, TableWriteKind, container_last_lba, load_gpt, parse_mbr, table_label,
};

/// 表事实与解析这张表所用的逻辑块大小。没有可读表（或表不可判定）时第二项取容器扇区，
/// 与在那之上新建表的口径一致。
/// 盘型判定复用 [`table_label`]：GPT 先于 MBR，保护布局受损的 GPT 不会被
/// 当成 msdos 盘读出另一套分区。
pub(crate) fn observe_table(src: &FileSource) -> Result<(TableObservation, u64), RunOutcome> {
    let label = match table_label(src) {
        Ok(label) => label,
        Err(GptError::Io(e)) => {
            return Err(RunOutcome::no_mutation(format!("cannot read the partition table: {e}")));
        }
        Err(e) => return Ok((TableObservation::Unusable { reason: e.to_string() }, src.sector_size)),
    };
    let mut table_ss = src.sector_size;
    let observation = match label {
        TableLabel::None => TableObservation::Absent,
        TableLabel::GptDamaged => TableObservation::Unusable {
            reason: "the target carries a damaged GPT layout; no usable table could be read".into(),
        },
        TableLabel::Gpt => match load_gpt(src) {
            Ok(Some(gpt)) => {
                table_ss = gpt.ss;
                TableObservation::Present(gpt_facts(&gpt))
            }
            Ok(None) => TableObservation::Unusable {
                reason: "the GPT disappeared between the label check and the table read".into(),
            },
            Err(GptError::Io(e)) => {
                return Err(RunOutcome::no_mutation(format!("cannot read the GPT: {e}")));
            }
            Err(e) => TableObservation::Unusable { reason: e.to_string() },
        },
        TableLabel::Mbr => match parse_mbr(src) {
            Ok(Some(parts)) => TableObservation::Present(mbr_facts(src, &parts)),
            Ok(None) => TableObservation::Unusable {
                reason: "the MBR disappeared between the label check and the table read".into(),
            },
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                TableObservation::Unusable { reason: e.to_string() }
            }
            Err(e) => return Err(RunOutcome::no_mutation(format!("cannot read the MBR: {e}"))),
        },
    };
    Ok((observation, table_ss))
}

fn gpt_facts(gpt: &RawGpt) -> TableFacts {
    let entries = gpt
        .entries
        .iter()
        .enumerate()
        // 空槽是 starting_lba 与 ending_lba 同为 0 的条目；装配层只应看见已定义的分区，
        // 否则容量与 :last 选择都会被整张空数组左右
        .filter(|(_, e)| e.ending_lba != 0)
        .map(|(i, e)| PartitionFact {
            number: (i + 1) as u32,
            first_lba: e.starting_lba,
            last_lba: e.ending_lba,
        })
        .collect();
    TableFacts {
        entries,
        first_usable: gpt.header.first_usable_lba,
        last_usable: gpt.header.last_usable_lba,
    }
}

fn mbr_facts(src: &FileSource, parts: &[MbrPartition]) -> TableFacts {
    let entries = parts
        .iter()
        .map(|p| PartitionFact {
            number: p.num,
            first_lba: p.start_lba as u64,
            last_lba: p.start_lba as u64 + p.size_lba as u64 - 1,
        })
        .collect();
    TableFacts {
        entries,
        // LBA0 是表自身；写入侧要求分区起点 ≥ 1（add_mdos_entry_to / resize_mdos_entry），
        // 末端 < 容器扇区数，故可用区是 1..=容器末 LBA
        first_usable: 1,
        last_usable: container_last_lba(src, src.sector_size),
    }
}

/// 计划期算好的新表内容。改动类别由计划给出，与内容分开：`kind` 只描述这次改动是什么，
/// 字节全部来自 `content`；`NewMbr` 是整表新建，没有内容可谈。
pub(crate) enum TableContent<'a> {
    /// GPT：新头、新条目数组与新的末 LBA，另可带保护 MBR 的整扇区新内容
    Gpt {
        kind: TableWriteKind,
        content: &'a GptContent,
        /// `Some` 时 LBA0 作为最后一段并入同一批写入；`None` 即不写 LBA0
        protective_mbr: Option<&'a [u8]>,
    },
    /// MBR：整扇区的新 LBA0
    Mbr { kind: TableWriteKind, lba0: &'a [u8] },
    /// 只重写保护 MBR：`content` 用于算出该表的四段比对区间，写入段只有 LBA0
    ProtectiveMbr { content: &'a GptContent, lba0: &'a [u8] },
    /// `new --table mbr`：LBA0、LBA1 与末 LBA
    NewMbr,
}

/// 已装配的表步骤。字段私有，唯一构造点是 [`assemble_table_step`]。
pub(crate) struct AssembledTableStep(TableStep);

impl AssembledTableStep {
    /// 交给步骤序列的唯一出口：`PlanStep::WriteTable` 携带的就是装配好的 `TableStep`。
    pub(crate) fn into_step(self) -> TableStep {
        self.0
    }
}

/// 计划期写前快照：按 [`TableContent`] 推出比对区间并一次读定。调用点必须在读出表内容
/// 之后立刻调用它，随后把快照交给 [`assemble_table_step`]/[`confirmed_table_step`]——
/// 装配层不再读盘，写前摘要就是这份快照里的 `stamp`。
pub(crate) fn take_table_snapshot(
    src: &FileSource,
    content: &TableContent<'_>,
) -> Result<TableSnapshot, RunOutcome> {
    let regions = match content {
        TableContent::Gpt { content, .. } => crate::table::gpt_compare_regions(src, content)?,
        TableContent::Mbr { .. } => crate::table::mbr_compare_regions(src)?,
        TableContent::ProtectiveMbr { content, .. } => crate::table::gpt_compare_regions(src, content)?,
        TableContent::NewMbr => crate::table::new_mbr_regions(src)?,
    };
    crate::table::table_snapshot(src, &regions)
}

/// 在计划期把表步骤装配完整：写前摘要取自调用方刚读定的 `snapshot`，`expect_after` 与
/// `writes` 由 [`TableContent`] 的字节算出。序列化与两处摘要都在 [`TableStep`] 的既有
/// 构造器里，本函数只按表族分派。
pub(crate) fn assemble_table_step(
    src: &FileSource,
    content: TableContent<'_>,
    snapshot: TableSnapshot,
) -> Result<AssembledTableStep, RunOutcome> {
    let step = match content {
        TableContent::Gpt { kind, content, protective_mbr } => {
            // 主数组声明位置 ≠ 规范位置（LBA2）时，提交会把数组归位并覆盖该区间的
            // 现有字节。归位本身是有意决策（位置字段由提交规范化），但覆盖非数组
            // 字节不能静默：写前比对已把原文登记进恢复记录，undo 可回放，这里只
            // 补足知情
            if content.header.partition_entry_lba != 2 {
                eprintln!(
                    "note: the on-disk GPT declares its primary entry array at LBA {}; committing \
                     normalizes it to LBA 2 and overwrites the bytes there (the pre-image is captured \
                     in the recovery record and can be rolled back with `diskedit undo`)",
                    content.header.partition_entry_lba
                );
            }
            TableStep::gpt(src, kind, content, protective_mbr, snapshot)?
        }
        TableContent::Mbr { kind, lba0 } => TableStep::mbr(src, kind, lba0, snapshot)?,
        TableContent::ProtectiveMbr { content, lba0 } => {
            TableStep::protective_mbr(src, content, lba0, snapshot)?
        }
        TableContent::NewMbr => TableStep::new_mbr(src, snapshot)?,
    };
    Ok(AssembledTableStep(step))
}

/// 只有一条已装配表步骤的计划，交给 [`crate::execution::execute_plan`] 落盘。
///
/// 步骤的确认门在解析期已由 `--yes` 判定（如 `delete`），占用闸由命令层在此之前判定；
/// 本函数只把装配好的步骤装进计划骨架，不改动任何判据。
pub(crate) fn confirmed_table_step(
    src: &FileSource,
    content: TableContent<'_>,
    snapshot: TableSnapshot,
) -> Result<ConfirmedPlan, RunOutcome> {
    let step = assemble_table_step(src, content, snapshot)?.into_step();
    let sector_size = SectorSize::try_new(src.sector_size).map_err(|e| {
        RunOutcome::unsupported(format!(
            "the target's sector size is outside the supported set — geometry cannot be constructed: {e}"
        ))
    })?;
    Ok(ConfirmedPlan::assembled_table_step(step, sector_size))
}

/// 修复动作的计划装配：一条表步骤，内容由动作与盘上表算定。
///
/// 判定在这里按当前盘面重做一次：调用方的 [`RepairAction`] 来自更早的读取，装配与执行
/// 之间的外部改动必须拒绝（3），不能拿旧动作覆盖新表。装配期只读；写入由
/// `execution::execute_plan_complete` 完成，写前/写后比对与其他表步骤同一套。
pub(crate) fn assemble_repair_plan(
    src: &FileSource,
    action: &crate::gpt_policy::RepairAction,
) -> Result<ConfirmedPlan, RunOutcome> {
    use crate::gpt_policy::RepairAction;
    let g = load_gpt(src)
        .map_err(RunOutcome::from)?
        .ok_or_else(|| RunOutcome::refused("GPT vanished before repair"))?;
    let file_last_lba = container_last_lba(src, g.ss);
    let now = crate::gpt_policy::classify_repair(&g, file_last_lba)
        .map_err(|e| RunOutcome::refused(e.to_string()))?;
    if now != *action {
        return Err(RunOutcome::refused(
            "GPT changed between validation and repair — re-run the command",
        ));
    }
    match action {
        RepairAction::None => Err(RunOutcome::internal(
            "a repair plan was requested for a no-op action",
        )),
        // 搬迁备份头：重写双头；保护 MBR 需要时并入同一表步
        RepairAction::RelocateBackup { new_last_usable, .. }
        | RepairAction::RelocateAndRepair { new_last_usable, .. } => {
            let vg = ValidatedGeometry::new(&g, file_last_lba, Some(*new_last_usable))
                .map_err(|e| RunOutcome::refused(e.to_string()))?;
            let content = vg.content();
            let protective_mbr = crate::table::protective_mbr_content(src, g.ss)?;
            let probe = TableContent::Gpt {
                kind: TableWriteKind::Repair,
                content: &content,
                protective_mbr: protective_mbr.as_deref(),
            };
            let snapshot = take_table_snapshot(src, &probe)?;
            confirmed_table_step(src, probe, snapshot)
        }
        // 仅保护 MBR：四段保持原样，写入段只有 LBA0
        RepairAction::RepairProtectiveMbr => {
            let lba0 = crate::table::protective_mbr_content(src, g.ss)?.ok_or_else(|| {
                RunOutcome::internal(
                    "the protective MBR is already canonical — the repair action does not match the disk",
                )
            })?;
            let content = GptContent {
                ss: g.ss,
                header: g.header.clone(),
                entries: g.entries.clone(),
                last_lba: file_last_lba,
            };
            let probe = TableContent::ProtectiveMbr { content: &content, lba0: &lba0 };
            let snapshot = take_table_snapshot(src, &probe)?;
            confirmed_table_step(src, probe, snapshot)
        }
    }
}

/// `resizefs` 离线形式的步骤装配：只含一条 `FsResize`。几何取表的扇区口径——条目 LBA 与
/// 长度按解析这张表所用的逻辑块大小换算（4Kn 表放在 512e 容器上时两者不同），`target_bytes`
/// 由 [`ConfirmedPlan::assembled_fs_resize`] 从 `sectors × 表扇区` 算出。要动的那一段与其中
/// 的类型由命令层在此之前用 `grow_target_at` 判定；本函数只读表，不重新识别内容。
pub(crate) fn assemble_fs_resize(
    src: &FileSource,
    part: u32,
    fstype: Option<ContentKind>,
) -> Result<ConfirmedPlan, RunOutcome> {
    let (table, table_ss) = observe_table(src)?;
    let TableObservation::Present(facts) = table else {
        return Err(RunOutcome::refused(format!(
            "`resizefs` needs a readable partition table on the target (partition #{part} was resolved \
             before the table became unreadable)"
        )));
    };
    let entry = facts.entries.iter().find(|e| e.number == part).ok_or_else(|| {
        RunOutcome::refused(format!(
            "partition #{part} left the partition table while the plan was being assembled"
        ))
    })?;
    let sector_size = SectorSize::try_new(table_ss).map_err(|e| {
        RunOutcome::unsupported(format!(
            "the table's sector size is outside the supported set — geometry cannot be constructed: {e}"
        ))
    })?;
    ConfirmedPlan::assembled_fs_resize(part, entry.first_lba, entry.sectors(), fstype, sector_size)
        .ok_or_else(|| {
            RunOutcome::internal(format!(
                "partition #{part}: the sector count overflows the address space"
            ))
        })
}

/// `mkfs` 的步骤装配：只含一条 `FsCreate`。几何取表的扇区口径——条目 LBA 与长度按解析
/// 这张表所用的逻辑块大小换算（4Kn 表放在 512e 容器上时两者不同）。要建的类型与卷标由
/// 命令层在此之前判定并传入；本函数只读表，不重新解析请求。
pub(crate) fn assemble_fs_create(
    src: &FileSource,
    part: u32,
    fstype: crate::fsops::MkfsFs,
    label: Option<String>,
) -> Result<ConfirmedPlan, RunOutcome> {
    let (table, table_ss) = observe_table(src)?;
    let TableObservation::Present(facts) = table else {
        return Err(RunOutcome::refused(format!(
            "`mkfs` needs a readable partition table on the target (partition #{part} was resolved \
             before the table became unreadable)"
        )));
    };
    let entry = facts.entries.iter().find(|e| e.number == part).ok_or_else(|| {
        RunOutcome::refused(format!(
            "partition #{part} left the partition table while the plan was being assembled"
        ))
    })?;
    let sector_size = SectorSize::try_new(table_ss).map_err(|e| {
        RunOutcome::unsupported(format!(
            "the table's sector size is outside the supported set — geometry cannot be constructed: {e}"
        ))
    })?;
    Ok(ConfirmedPlan::assembled_fs_create(
        part,
        entry.first_lba,
        entry.sectors(),
        fstype,
        label,
        sector_size,
    ))
}

/// `set` 的 FS 属性请求。UUID 的两种意图在装配期按目标能力落地：能接受具体值的 FS
/// 在这里取系统随机源生成文本，只能自生成的 FS（ntfs）把生成动作留给工具。
pub(crate) enum FsSetRequest {
    Label(String),
    Uuid(crate::fsops::UuidRequest),
}

/// `set` 的 label/uuid 步骤装配：只含一条 `FsSet`。要改的属性、最终值与类型由命令层
/// 在此之前判定并传入；本函数只确认分区仍在可读的表里，不重新识别分区内容。
pub(crate) fn assemble_fs_set(
    src: &FileSource,
    part: u32,
    fstype: Option<ContentKind>,
    request: FsSetRequest,
) -> Result<ConfirmedPlan, RunOutcome> {
    let (table, table_ss) = observe_table(src)?;
    let TableObservation::Present(facts) = table else {
        return Err(RunOutcome::refused(format!(
            "`set` needs a readable partition table on the target (partition #{part} was resolved \
             before the table became unreadable)"
        )));
    };
    if !facts.entries.iter().any(|e| e.number == part) {
        return Err(RunOutcome::refused(format!(
            "partition #{part} left the partition table while the plan was being assembled"
        )));
    }
    let sector_size = SectorSize::try_new(table_ss).map_err(|e| {
        RunOutcome::unsupported(format!(
            "the table's sector size is outside the supported set — geometry cannot be constructed: {e}"
        ))
    })?;
    let attr = match request {
        FsSetRequest::Label(label) => FsSetAttr::Label(label),
        FsSetRequest::Uuid(request) => FsSetAttr::Uuid(uuid_value(fstype, request)?),
    };
    Ok(ConfirmedPlan::assembled_fs_set(part, attr, fstype, sector_size))
}

/// UUID 请求 → 载荷：`Generate` 只留给只能自生成的 FS（ntfs）；其余目标按具体值写入，
/// 包括 `--random` 在这里解析出的新文本。请求与能力的匹配已由命令层判定，这里不再拒绝
fn uuid_value(
    fstype: Option<ContentKind>,
    request: crate::fsops::UuidRequest,
) -> Result<UuidValue, RunOutcome> {
    match request {
        crate::fsops::UuidRequest::Explicit(text) => Ok(UuidValue::Explicit(text)),
        crate::fsops::UuidRequest::NewRandom => match crate::fsops::uuid_support(fstype) {
            crate::fsops::UuidSupport::GeneratedOnly => Ok(UuidValue::Generate),
            _ => Ok(UuidValue::Explicit(new_uuid_text()?)),
        },
    }
}

/// 新的 FS UUID：16 字节取自系统随机源，按 RFC 4122 v4 置版本位与 variant 位，输出
/// 标准文本（8-4-4-4-12，小写）。随机源不可用时尚未写盘，按未写盘故障上报。
fn new_uuid_text() -> Result<String, RunOutcome> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| {
        RunOutcome::no_mutation(format!(
            "cannot read the system random source for a new UUID: {e}"
        ))
    })?;
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 0x0F) as usize] as char);
    }
    Ok(text)
}

/// GPT `add` 的表步骤装配：条目已写入 `geometry` 之后，按几何算新表内容，紧接内容读定
/// 写前快照，再取保护性 MBR 的规范化内容（已合规即 `None`，不写 LBA0），装配为单个
/// `WriteTable(Add)`。比对区间含 LBA0、LBA1、主项数组、备份项数组与末 LBA；写入段为
/// 备份项数组、备份头、主项数组、主头，保护性 MBR 待写时追加 LBA0。范围、重叠与槽位
/// 判定由调用方在此之前完成。
pub(crate) fn assemble_gpt_add(
    src: &FileSource,
    geometry: &ValidatedGeometry,
) -> Result<ConfirmedPlan, RunOutcome> {
    let step = gpt_add_step(src, geometry)?;
    let sector_size = SectorSize::try_new(src.sector_size).map_err(|e| {
        RunOutcome::unsupported(format!(
            "the target's sector size is outside the supported set — geometry cannot be constructed: {e}"
        ))
    })?;
    Ok(ConfirmedPlan::assembled_table_step(step, sector_size))
}

/// 「条目已就位」的几何 → 装配好的 `WriteTable(Add)`：`add` 与 `copy` 的表步骤同源。
/// 写前快照在算出内容之后立刻读定，保护 MBR 的读取排在快照之后——它若在快照之后被
/// 第三方改动，写前按 regions 比对会落 5，不会写出与快照不符的内容。
fn gpt_add_step(src: &FileSource, geometry: &ValidatedGeometry) -> Result<TableStep, RunOutcome> {
    let content = geometry.content();
    let probe = TableContent::Gpt {
        kind: TableWriteKind::Add,
        content: &content,
        protective_mbr: None,
    };
    let snapshot = take_table_snapshot(src, &probe)?;
    let protective_mbr = crate::table::protective_mbr_content(src, geometry.sector_size())?;
    let view = TableContent::Gpt {
        kind: TableWriteKind::Add,
        content: &content,
        protective_mbr: protective_mbr.as_deref(),
    };
    Ok(assemble_table_step(src, view, snapshot)?.into_step())
}

/// `copy` 的多步计划装配：`CopyPartition` 在前、提交副本条目的表步骤在后，顺序是
/// 「先搬数据、后写表」。副本条目已由调用方写进 `geometry`（与 `add` 共用几何
/// 准备），表步骤内容与 `add` 同源。计划的扇区口径取表自身的 ss：复制按表的 LBA 换算
/// 字节。表步骤与复制步骤成对，执行层因此能在写任何数据字节之前比对表区。
pub(crate) fn assemble_copy(
    src: &FileSource,
    geometry: &ValidatedGeometry,
    prepared: &crate::movepart::PreparedCopy,
) -> Result<ConfirmedPlan, RunOutcome> {
    let table = gpt_add_step(src, geometry)?;
    let sector_size = SectorSize::try_new(geometry.sector_size()).map_err(|e| {
        RunOutcome::unsupported(format!(
            "the table's sector size is outside the supported set — geometry cannot be constructed: {e}"
        ))
    })?;
    ConfirmedPlan::assembled_copy(
        PlanStep::CopyPartition {
            part: prepared.part,
            src_first: prepared.src_start,
            dst_first: prepared.new_start,
            sectors: prepared.len,
        },
        table,
        sector_size,
    )
    .ok_or_else(|| {
        RunOutcome::internal(
            "the assembled copy steps are not a CopyPartition followed by a stamped table step",
        )
    })
}

/// `resize-part` 的表步骤：`geometry` 已经带着请求的新末端，本函数按几何算新表内容，
/// 紧接内容读定写前快照，再取保护性 MBR 的规范化内容，装配为单步 `WriteTable`。
/// 区间、重叠与确认门由命令层在此之前判定；`FsResize` 收尾另由 [`assemble_fs_resize`]
/// 在表落盘之后装配。计划的扇区口径取表自身的 ss。
pub(crate) fn assemble_resize_part(
    src: &FileSource,
    geometry: &ValidatedGeometry,
    kind: TableWriteKind,
    relocate: Option<(u32, u64, u64, u64)>,
) -> Result<ConfirmedPlan, RunOutcome> {
    let content = geometry.content();
    let probe = TableContent::Gpt {
        kind,
        content: &content,
        protective_mbr: None,
    };
    let snapshot = take_table_snapshot(src, &probe)?;
    // 保护 MBR 的读取排在快照之后：它若在快照之后被第三方改动，写前按 regions 比对会落 5
    let protective_mbr = crate::table::protective_mbr_content(src, geometry.sector_size())?;
    let view = TableContent::Gpt {
        kind,
        content: &content,
        protective_mbr: protective_mbr.as_deref(),
    };
    let step = assemble_table_step(src, view, snapshot)?.into_step();
    let sector_size = SectorSize::try_new(geometry.sector_size()).map_err(|e| {
        RunOutcome::unsupported(format!(
            "the table's sector size is outside the supported set — geometry cannot be constructed: {e}"
        ))
    })?;
    // 起点变更的搬移在前、表提交在后：分块、方向与 durable 游标由执行器的 Relocate
    // 机制承担，与 move 同一条链；纯扩缩只有表步骤
    let mut steps: Vec<crate::planner::PlanStep> = Vec::new();
    if let Some((part, src_first, dst_first, sectors)) = relocate {
        steps.push(crate::planner::PlanStep::Relocate {
            part,
            src_first,
            dst_first,
            sectors,
            is_swap: false,
        });
    }
    steps.push(crate::planner::PlanStep::WriteTable(step));
    crate::planner::ConfirmedPlan::assembled_move_plan(steps, sector_size).ok_or_else(|| {
        RunOutcome::internal("the resize-part plan assembled to an invalid step shape — refusing".to_string())
    })
}

/// `set` 的表属性装配：读几何 → 在计划内容上落实本次变更 → 首写前复核条目仍是决策期
/// 读到的那份 → 紧接内容读定写前快照 → 需要修复时取保护性 MBR 的规范化内容 →
/// 单步 `WriteTable`。
///
/// 变更只碰条目字段，分区区间不变；`Replace` 表示整表重写，不增删条目也不改长度。
///
/// 盘上需要修复的表由本步骤一并落盘：内容取修复后的头与末 LBA（[`ValidatedGeometry`]
/// 已收敛），LBA0 只在本次同时承担修复时按规范化内容重写。
fn gpt_attribute_plan(
    src: &FileSource,
    edit: impl FnOnce(&mut GptContent) -> Result<(), RunOutcome>,
) -> Result<ConfirmedPlan, RunOutcome> {
    let (geometry, repair) = crate::gpt_policy::resolve_geometry(src)?
        .ok_or_else(|| RunOutcome::refused("no GPT"))?;
    let mut content = geometry.content();
    edit(&mut content)?;
    // 决策几何读定之后条目被第三方改过即拒绝：此刻一个字节都没写
    geometry.ensure_entries_unchanged_fail(src)?;
    let kind = TableWriteKind::Replace;
    let probe = TableContent::Gpt {
        kind,
        content: &content,
        protective_mbr: None,
    };
    let snapshot = take_table_snapshot(src, &probe)?;
    // 无修复动作时 LBA0 一字不动：保护 MBR 的规范化归属于修复相位，只有本次步骤
    // 同时承担修复时才取它的内容。读取排在快照之后：它若在快照之后被第三方改动，
    // 写前按 regions 比对会落 5
    let protective_mbr = match repair {
        crate::gpt_policy::RepairAction::None => None,
        _ => crate::table::protective_mbr_content(src, content.ss)?,
    };
    let view = TableContent::Gpt {
        kind,
        content: &content,
        protective_mbr: protective_mbr.as_deref(),
    };
    let step = assemble_table_step(src, view, snapshot)?.into_step();
    let sector_size = SectorSize::try_new(content.ss).map_err(|e| {
        RunOutcome::unsupported(format!(
            "the table's sector size is outside the supported set — geometry cannot be constructed: {e}"
        ))
    })?;
    Ok(ConfirmedPlan::assembled_table_step(step, sector_size))
}

/// `set name` 的 GPT 表步骤：只含一条 `WriteTable`。名字长度与可打印字符判据由命令层
/// 在此之前判定
pub(crate) fn assemble_set_name(
    src: &FileSource,
    part: u32,
    name: &str,
) -> Result<ConfirmedPlan, RunOutcome> {
    gpt_attribute_plan(src, |content| {
        crate::gpt_policy::rename_content(content, part, name)
    })
}

/// `set type` 的 GPT 表步骤：类型 GUID 已由命令层解析；全零拒绝在读几何之前给出
pub(crate) fn assemble_set_gpt_type(
    src: &FileSource,
    part: u32,
    guid: [u8; 16],
) -> Result<ConfirmedPlan, RunOutcome> {
    crate::gpt_policy::check_type_guid(guid)?;
    gpt_attribute_plan(src, |content| {
        crate::gpt_policy::set_type_content(content, part, guid)
    })
}

/// `set flag` 的 GPT 表步骤：`esp off` 的回退落点在装配期读盘识别 FS（只读），结论作为
/// 参数交给内容变更，步骤里不再读盘
pub(crate) fn assemble_set_gpt_flag(
    src: &FileSource,
    part: u32,
    flag: GptFlag,
    on: bool,
) -> Result<ConfirmedPlan, RunOutcome> {
    gpt_attribute_plan(src, |content| {
        let fallback = crate::gpt_policy::esp_off_fallback_for(src, content, part, flag, on)?;
        crate::gpt_policy::set_flag_content(content, part, flag, on, fallback)
    })
}

/// msdos 表属性：LBA0 原文经纯函数算出新内容之后，紧接内容读定写前快照，装配为单步
/// `WriteTable`。校验、槽位与条目判据由命令层与 [`crate::table`] 的纯函数在此之前给出
fn mbr_attribute_plan(src: &FileSource, lba0: &[u8]) -> Result<ConfirmedPlan, RunOutcome> {
    let kind = TableWriteKind::Replace;
    let snapshot = take_table_snapshot(src, &TableContent::Mbr { kind, lba0 })?;
    confirmed_table_step(src, TableContent::Mbr { kind, lba0 }, snapshot)
}

/// `set type` 的 msdos 表步骤：类型字节的写前判据先于读盘
pub(crate) fn assemble_set_mbr_type(
    src: &FileSource,
    part: u32,
    t: u8,
) -> Result<ConfirmedPlan, RunOutcome> {
    crate::table::check_mdos_type(part, t)?;
    let lba0 = crate::table::read_mbr_lba0(src)?;
    let out = crate::table::set_mdos_type_image(&lba0, part, t)?;
    mbr_attribute_plan(src, &out)
}

/// `set flag boot` 的 msdos 表步骤
pub(crate) fn assemble_set_mbr_boot(
    src: &FileSource,
    part: u32,
    on: bool,
) -> Result<ConfirmedPlan, RunOutcome> {
    crate::table::primary_slot(part)?;
    let lba0 = crate::table::read_mbr_lba0(src)?;
    let out = crate::table::set_mdos_boot_image(&lba0, part, on)?;
    mbr_attribute_plan(src, &out)
}

/// `set flag hidden` 的 msdos 表步骤：条目已在目标态时没有写盘内容，返回 `None`
pub(crate) fn assemble_set_mbr_hidden(
    src: &FileSource,
    part: u32,
    on: bool,
) -> Result<Option<ConfirmedPlan>, RunOutcome> {
    crate::table::primary_slot(part)?;
    let lba0 = crate::table::read_mbr_lba0(src)?;
    let Some(out) = crate::table::set_mdos_hidden_image(&lba0, part, on)? else {
        return Ok(None);
    };
    Ok(Some(mbr_attribute_plan(src, &out)?))
}

/// 多步搬移计划的装配：按 `moves` 的顺序逐条目推进 `geometry`，每个条目得到
/// 「`Relocate` + 该条目的表提交」一对。`moves` 的每一项是 1-based 分区号与目标起始
/// LBA；顺序即执行顺序，落点与尚未搬移的条目重叠时 [`ValidatedGeometry::update_entry`]
/// 拒绝该次推进。
///
/// 第 i 个条目的表内容取自「条目 0..=i 已就位」的几何，表步骤的两端表戳随之一条链：
/// 第一个表步骤的 `expect_before` 是调用时盘上的表区快照，之后每个表步骤的
/// `expect_before` 等于前一个表步骤的 `expect_after`。链不一致按内部错误拒绝。
///
/// swap 条目不在这里装配 `Relocate`：搬移执行器不重建 swap 内容，遇到时返回 6（能力缺口）、
/// 不产出计划。类型 GUID 为 LUKS / LVM PV 的条目只允许改尺寸，搬移按请求不成立返回 3。
/// 保护性 MBR 由 [`crate::table::protective_mbr_content`] 判定，需要规范化时并入每个表步骤
/// 的 LBA0 写入段，已合规时不写 LBA0。本函数不产 `FsResize` / `FsCreate` / repair 步骤。
pub(crate) fn assemble_relocation_plan(
    src: &FileSource,
    geometry: &mut ValidatedGeometry,
    moves: &[(u32, u64)],
) -> Result<ConfirmedPlan, RunOutcome> {
    assemble_move_steps(src, geometry, moves, None, false)
}

/// `resize --allow-move` 的整份已装配计划：搬移 + 表提交 + 换区重建，末尾是把目标分区
/// 扩到 `grow_end` 的表步骤。
///
/// 与 [`assemble_relocation_plan`] 是同一条装配链，差别只在两处：换区条目允许出现
/// （内容可弃，表提交之后多一步 `RebuildSwap`），以及末尾带一个扩容表步骤。扩容与搬移
/// 同属一份作业，表戳链因此必须一路串到最后一步——续跑时前一步写完的表正是后一步的
/// 写前快照。
pub(crate) fn assemble_resize_move_plan(
    src: &FileSource,
    geometry: &mut ValidatedGeometry,
    moves: &[(u32, u64)],
    grow: (u32, u64),
) -> Result<ConfirmedPlan, RunOutcome> {
    assemble_move_steps(src, geometry, moves, Some(grow), true)
}

/// 搬移作业的装配内核：`moves` 按执行顺序给出（最右侧先搬），`grow` 是末尾那次扩容
/// 的 `(分区号, 新末端)`。`allow_swap` 决定换区条目是"装配一步重建"还是"拒绝"。
fn assemble_move_steps(
    src: &FileSource,
    geometry: &mut ValidatedGeometry,
    moves: &[(u32, u64)],
    grow: Option<(u32, u64)>,
    allow_swap: bool,
) -> Result<ConfirmedPlan, RunOutcome> {
    if moves.is_empty() && grow.is_none() {
        return Err(RunOutcome::internal(
            "a move plan needs at least one entry — nothing was assembled",
        ));
    }
    let initial = geometry.content();
    let mut state = take_table_snapshot(
        src,
        &TableContent::Gpt {
            kind: TableWriteKind::Repartition,
            content: &initial,
            protective_mbr: None,
        },
    )?;
    // 快照先读定、保护 MBR 后读：若 LBA0 在两者之间被改动，写前按 regions 比对会落 5，
    // 不会把旧 LBA0 的引导代码写回去
    let protective_mbr = crate::table::protective_mbr_content(src, initial.ss)?;
    let mut previous_after: Option<TableStamp> = None;
    let mut steps: Vec<PlanStep> = Vec::with_capacity(moves.len() * 3 + 1);
    for &(part, dst_first) in moves {
        let (src_first, sectors, is_swap) = move_entry(geometry, part)?;
        if is_swap && !allow_swap {
            return Err(RunOutcome::unsupported(format!(
                "partition #{part} is a swap partition — the relocation executor does not rebuild swap contents"
            )));
        }
        let end = dst_first
            .checked_add(sectors - 1)
            .ok_or_else(|| RunOutcome::refused(format!("partition #{part} would end beyond the LBA range")))?;
        let index = geometry.entry_index(part).ok_or_else(|| {
            RunOutcome::refused(format!("partition #{part} is outside the entry array"))
        })?;
        geometry
            .update_entry(index, |e| {
                e.starting_lba = dst_first;
                e.ending_lba = end;
            })
            .map_err(|e| RunOutcome::refused(format!("partition #{part} cannot move to LBA {dst_first}: {e}")))?;
        let step = assemble_after(src, geometry, &protective_mbr, &mut state, &mut previous_after)?;
        steps.push(PlanStep::Relocate { part, src_first, dst_first, sectors, is_swap });
        steps.push(PlanStep::WriteTable(step));
        if is_swap {
            steps.push(PlanStep::RebuildSwap { part, src_first, dst_first, sectors });
        }
    }
    if let Some((part, grow_end)) = grow {
        let index = geometry.entry_index(part).ok_or_else(|| {
            RunOutcome::refused(format!("partition #{part} is outside the entry array"))
        })?;
        geometry.update_entry(index, |e| e.ending_lba = grow_end).map_err(|e| {
            RunOutcome::refused(format!("partition #{part} cannot grow to LBA {grow_end}: {e}"))
        })?;
        let step = assemble_after(src, geometry, &protective_mbr, &mut state, &mut previous_after)?;
        steps.push(PlanStep::WriteTable(step));
    }
    // 计划里的 LBA 全部来自分区表，按表扇区换算字节；容器扇区只用于把容器尺寸换成表 LBA
    let sector_size = SectorSize::try_new(geometry.sector_size()).map_err(|e| {
        RunOutcome::unsupported(format!(
            "the table's sector size is outside the supported set — geometry cannot be constructed: {e}"
        ))
    })?;
    ConfirmedPlan::assembled_move_plan(steps, sector_size).ok_or_else(|| {
        RunOutcome::internal(
            "the assembled move steps do not have the (Relocate, WriteTable, RebuildSwap?)+, WriteTable? shape",
        )
    })
}

/// 一条被搬移条目的当前几何：盘上起点、长度与它是不是换区。三种拒绝在这里给出——
/// 不在条目数组里、未定义、端点倒挂、LUKS/LVM PV 不可搬移
fn move_entry(
    geometry: &ValidatedGeometry,
    part: u32,
) -> Result<(u64, u64, bool), RunOutcome> {
    let index = geometry
        .entry_index(part)
        .ok_or_else(|| RunOutcome::refused(format!("partition #{part} is outside the entry array")))?;
    let entry = geometry.entries().get(index).ok_or_else(|| {
        RunOutcome::internal(format!("partition #{part} has no entry in the geometry"))
    })?;
    if entry.starting_lba == 0 && entry.ending_lba == 0 {
        return Err(RunOutcome::refused(format!("partition #{part} is not defined")));
    }
    if entry.starting_lba > entry.ending_lba {
        return Err(RunOutcome::internal(format!(
            "partition #{part} has an inverted range {}..{}",
            entry.starting_lba, entry.ending_lba
        )));
    }
    if is_forbidden_type_guid(&entry.partition_type_guid) {
        return Err(RunOutcome::refused(format!(
            "partition #{part} is a LUKS / LVM PV partition — this tool does not relocate it, only size \
             changes are allowed"
        )));
    }
    Ok((
        entry.starting_lba,
        entry.ending_lba - entry.starting_lba + 1,
        is_swap_guid(&entry.partition_type_guid),
    ))
}

/// 按当前几何装配一个表步骤，并把表戳链接上下一个。调用方保证几何刚被推进过
fn assemble_after(
    src: &FileSource,
    geometry: &ValidatedGeometry,
    protective_mbr: &Option<Vec<u8>>,
    state: &mut TableSnapshot,
    previous_after: &mut Option<TableStamp>,
) -> Result<TableStep, RunOutcome> {
    let content = geometry.content();
    let snapshot = TableSnapshot {
        stamp: state.stamp,
        regions: state.regions.clone(),
        bytes: state.bytes.clone(),
    };
    let step = assemble_table_step(
        src,
        TableContent::Gpt {
            kind: TableWriteKind::Repartition,
            content: &content,
            protective_mbr: protective_mbr.as_deref(),
        },
        snapshot,
    )?
    .into_step();
    if let Some(previous) = *previous_after
        && step.expect_before != Some(previous)
    {
        return Err(RunOutcome::internal(
            "the assembled table steps do not chain: a step's pre-write stamp differs from the \
             previous step's post-write stamp",
        ));
    }
    *previous_after = step.expect_after;
    *state = snapshot_after(state.clone(), &step)?;
    Ok(step)
}

/// 一个已装配表步骤执行之后表区的状态：写入段取该步骤的字节，其余区间沿用执行前的
/// 原文，表戳取该步骤的 `expect_after`。下一步装配的 `expect_before` 从这份状态取；
/// 纯值，不读盘。
fn snapshot_after(mut before: TableSnapshot, step: &TableStep) -> Result<TableSnapshot, RunOutcome> {
    if before.regions != step.regions {
        return Err(RunOutcome::internal(
            "the table step compares a different range than the state it was assembled from",
        ));
    }
    let after = step
        .expect_after
        .ok_or_else(|| RunOutcome::internal("the table step carries no post-write stamp"))?;
    for (region, bytes) in before.regions.iter().zip(before.bytes.iter_mut()) {
        if let Some(write) = step.writes.iter().find(|w| w.region == *region) {
            *bytes = write.bytes.clone();
        }
    }
    before.stamp = after;
    Ok(before)
}
