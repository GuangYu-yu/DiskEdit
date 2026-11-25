//! `add` / `delete` / `set` 的判定：把请求落成一份整表写入计划。
//!
//! 三条命令共用同一条路径：先确认目标空闲且表可编辑，再在纯核里算出新表字节，
//! 最后把"写哪几段、写前摘要是什么"交给执行层。判定层不碰句柄，也不决定写。
//!
//! `set label` 与 `set uuid` 写的是文件系统内容而不是分区表，那部分随文件系统
//! 执行路径一起落地；这里给出明确的不可用拒绝，而不是静默降级。

use diskedit_core::table::Guid;
use diskedit_core::table::create::Segment;
use diskedit_core::table::edit::{
    self, BASIC_DATA_TYPE_GUID, EditError, GptFlag, GptTable, LINUX_FS_TYPE_GUID, LINUX_MBR_TYPE,
    MbrFlag,
};
use diskedit_core::table::gpt::entries::PartitionEntry;

use crate::app::observe::edit::{EditSnapshot, TableFacts};
use crate::app::observe::table::TableView;
use crate::app::observe::{
    Alignment, Command, PartitionSelector, Request, SetKey, SetRequest, StartSpec, TableFormat,
    TypeSelector,
};

use super::step::{Step, StepKind, TableSegment};
use super::{Decision, Plan, Refusal};

/// 未给出 `--chunk` 时的默认块大小。
const DEFAULT_CHUNK: u64 = 4 << 20;
/// 1 MiB 的字节数。
const MIB: u64 = 1 << 20;

/// 判定 `move`：保持长度，把分区内容与条目平移到新起点。
pub fn move_partition(snapshot: &EditSnapshot, request: &Request) -> Decision {
    let Command::Move(moved) = request.command() else {
        return refuse_edit("move reached with a different command");
    };
    if let Some(refusal) = precheck(snapshot, "move") {
        return Decision::Refuse(refusal);
    }
    let Some(identity) = snapshot.target.identity else {
        return Decision::Refuse(Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        });
    };
    let Some(TableFacts::Gpt {
        header,
        entries,
        sector_size,
        total_bytes,
    }) = &snapshot.facts
    else {
        return Decision::Refuse(Refusal::GptRequired {
            what: "`move` needs a GPT partition table".to_owned(),
        });
    };
    let table = GptTable {
        header: header.clone(),
        entries: entries.clone(),
        sector_size: *sector_size,
        total_bytes: *total_bytes,
    };
    let Some(number) = resolve_gpt(&table, moved.partition) else {
        return refuse_edit("there is no partition to point `:last` at");
    };
    let index = match table.live_index(number) {
        Ok(index) => index,
        Err(error) => return refuse_edit(&error.to_string()),
    };
    let entry = table.entries[index].clone();
    let old_start = entry.starting_lba;
    let len = entry.ending_lba - old_start + 1;
    let new_start = match start_lba(&table, moved.start, moved.align, old_start, Some(len)) {
        Ok(start) => start,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    if new_start == old_start {
        return Decision::Unchanged {
            note: format!(
                "partition #{number} already starts at LBA {new_start} — nothing to move"
            ),
        };
    }
    let Some(new_end) = new_start.checked_add(len - 1) else {
        return refuse_edit("the move target overflows the LBA range");
    };
    if let Err(refusal) = place_range(&table, index, new_start, new_end, true) {
        return Decision::Refuse(refusal);
    }
    let shift = i64::try_from(i128::from(new_start) - i128::from(old_start)).unwrap_or(i64::MAX);
    let chunk = moved.chunk.unwrap_or(DEFAULT_CHUNK);
    let mut updated = table.entries.clone();
    updated[index].starting_lba = new_start;
    updated[index].ending_lba = new_end;
    let segments = match table.segments(&updated) {
        Ok(segments) => segments,
        Err(error) => {
            return Decision::Refuse(Refusal::TableDamaged {
                why: error.to_string(),
            });
        }
    };
    let data = Step::new(
        StepKind::MovePartition {
            partition: number,
            source_lba: old_start,
            offset_sectors: shift,
            chunk,
            ntfs_hidden: false,
        },
        vec![region(new_start * sector_size, len * sector_size)],
    );
    let table_step = table_step(
        TableFormat::Gpt,
        *sector_size,
        segments,
        snapshot.stamp,
    );
    let plan = Plan::write(vec![data, table_step], identity);
    finish(
        snapshot,
        plan,
        format!(
            "moved partition #{number} to LBA {new_start} (verify with: diskedit info {})",
            snapshot.target.display
        ),
    )
}

/// 判定 `copy`：把源分区内容与条目复制到新起点，源保持不变。
pub fn copy_partition(snapshot: &EditSnapshot, request: &Request) -> Decision {
    let Command::Copy(copied) = request.command() else {
        return refuse_edit("copy reached with a different command");
    };
    if let Some(refusal) = precheck(snapshot, "copy") {
        return Decision::Refuse(refusal);
    }
    let Some(identity) = snapshot.target.identity else {
        return Decision::Refuse(Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        });
    };
    let Some(TableFacts::Gpt {
        header,
        entries,
        sector_size,
        total_bytes,
    }) = &snapshot.facts
    else {
        return Decision::Refuse(Refusal::GptRequired {
            what: "`copy` needs a GPT partition table".to_owned(),
        });
    };
    let table = GptTable {
        header: header.clone(),
        entries: entries.clone(),
        sector_size: *sector_size,
        total_bytes: *total_bytes,
    };
    let Some(number) = resolve_gpt(&table, copied.partition) else {
        return refuse_edit("there is no partition to point `:last` at");
    };
    let index = match table.live_index(number) {
        Ok(index) => index,
        Err(error) => return refuse_edit(&error.to_string()),
    };
    if copied.start == StartSpec::Align {
        return Decision::Refuse(Refusal::TableEdit {
            why: "`copy --start align` has no existing start to align — give the destination LBA or `end`"
                .to_owned(),
        });
    }
    let entry = table.entries[index].clone();
    let len = entry.ending_lba - entry.starting_lba + 1;
    let new_start = match start_lba(&table, copied.start, copied.align, entry.starting_lba, Some(len))
    {
        Ok(start) => start,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let Some(new_end) = new_start.checked_add(len - 1) else {
        return refuse_edit("the copy target overflows the LBA range");
    };
    // 副本占用新的条目槽位；目标区间与任何既有条目（含源）重叠都拒绝。
    if let Err(refusal) = place_range(&table, usize::MAX, new_start, new_end, false) {
        return Decision::Refuse(refusal);
    }
    let mut clone = entry.clone();
    if let Some(name) = &copied.name {
        if let Err(refusal) = check_name(name) {
            return Decision::Refuse(refusal);
        }
        clone.name = diskedit_core::table::gpt::entries::PartitionName::from_str_lossy(name);
    }
    clone.starting_lba = new_start;
    clone.ending_lba = new_end;
    if !copied.keep_partuuid {
        clone.unique_partition_guid = derive_unique(identity, new_start, new_end);
    }
    let slot = match table.add_slot(None, new_start, new_end) {
        Ok(slot) => slot,
        Err(error) => return refuse_edit(&error.to_string()),
    };
    let mut updated = table.entries.clone();
    updated[slot] = clone;
    let segments = match table.segments(&updated) {
        Ok(segments) => segments,
        Err(error) => {
            return Decision::Refuse(Refusal::TableDamaged {
                why: error.to_string(),
            });
        }
    };
    let data = Step::new(
        StepKind::CopyPartition {
            source: number,
            source_lba: entry.starting_lba,
            start_lba: new_start,
            sectors: len,
            chunk: copied.chunk.unwrap_or(DEFAULT_CHUNK),
        },
        vec![region(new_start * sector_size, len * sector_size)],
    );
    let table_step = table_step(
        TableFormat::Gpt,
        *sector_size,
        segments,
        snapshot.stamp,
    );
    let plan = Plan::write(vec![data, table_step], identity);
    let number = u32::try_from(slot + 1).expect("条目下标可表示");
    finish_copy(
        snapshot,
        plan,
        format!(
            "copied to partition #{number} (verify with: diskedit info {})",
            snapshot.target.display
        ),
    )
}

/// 起点选择：LBA 按 --align 上取整，`end` 贴末，`align` 取原起点之后的下一个 1 MiB 边界。
fn start_lba(
    table: &GptTable,
    start: StartSpec,
    align: Alignment,
    old_start: u64,
    length: Option<u64>,
) -> Result<u64, Refusal> {
    if align != Alignment::Default && matches!(start, StartSpec::End | StartSpec::Align) {
        return Err(Refusal::TableEdit {
            why: "`--align` applies only to an explicit --start LBA".to_owned(),
        });
    }
    let first = table.header.first_usable_lba;
    let last = table.header.last_usable_lba;
    let unit = MIB / table.sector_size.max(1);
    let aligned = |lba: u64| -> Result<u64, Refusal> {
        if align == Alignment::Mib && unit > 0 {
            lba.div_ceil(unit)
                .checked_mul(unit)
                .ok_or_else(|| Refusal::TableEdit {
                    why: "the aligned start overflows the LBA range".to_owned(),
                })
        } else {
            Ok(lba)
        }
    };
    let new_start = match start {
        StartSpec::Lba(lba) => aligned(lba)?,
        StartSpec::End => {
            let len = length.ok_or_else(|| Refusal::TableEdit {
                why: "`--start end` needs a known length".to_owned(),
            })?;
            last.checked_sub(len - 1)
                .ok_or_else(|| Refusal::TableEdit {
                    why: "the partition is longer than the usable range".to_owned(),
                })?
        }
        StartSpec::Align => {
            let unit = MIB / table.sector_size.max(1);
            if unit == 0 {
                old_start
            } else {
                old_start
                    .div_ceil(unit)
                    .checked_mul(unit)
                    .ok_or_else(|| Refusal::TableEdit {
                        why: "the aligned start overflows the LBA range".to_owned(),
                    })?
            }
        }
    };
    if new_start < first {
        return Err(Refusal::TableEdit {
            why: format!("the start LBA {new_start} is before the first usable LBA {first}"),
        });
    }
    Ok(new_start)
}

/// 校验目标区间在可用范围内，且不与应避开的既有条目相交。
///
/// `skip` 是要避开的条目下标（搬移自身可与之相交）；`usize::MAX` 表示不跳过任何条目。
fn place_range(
    table: &GptTable,
    skip: usize,
    start: u64,
    end: u64,
    allow_self: bool,
) -> Result<(), Refusal> {
    let first = table.header.first_usable_lba;
    let last = table.header.last_usable_lba;
    if start > end || start < first || end > last {
        return Err(Refusal::TableEdit {
            why: format!("the target range {start}..{end} is outside the usable {first}..{last}"),
        });
    }
    for (index, entry) in table.entries.iter().enumerate() {
        if entry.partition_type_guid.is_zero() {
            continue;
        }
        if allow_self && index == skip {
            continue;
        }
        if !(end < entry.starting_lba || start > entry.ending_lba) {
            return Err(Refusal::PartitionOverlap {
                number: u32::try_from(index + 1).unwrap_or(u32::MAX),
            });
        }
    }
    Ok(())
}

/// 组装数据步之后的整表写入步。
fn table_step(
    format: TableFormat,
    segment_size: u64,
    segments: Vec<Segment>,
    stamp: [u8; 32],
) -> Step {
    let regions = segments.iter().map(|segment| segment.region).collect();
    let segments: Vec<TableSegment> = segments
        .into_iter()
        .map(|segment| TableSegment {
            region: segment.region,
            bytes: segment.bytes,
        })
        .collect();
    Step::new(
        StepKind::WriteTable {
            format,
            segment_size,
            stamp,
            segments,
        },
        regions,
    )
}

/// `copy` 不接受续跑：目标上已有未收尾记录一律拒绝。
fn finish_copy(snapshot: &EditSnapshot, plan: Result<Plan, Refusal>, note: String) -> Decision {
    let plan = match plan {
        Ok(plan) => plan,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let path = record_path(snapshot);
    match &snapshot.prior {
        crate::app::observe::PriorState::Absent => {}
        crate::app::observe::PriorState::Unreadable => {
            return Decision::Refuse(Refusal::RecordUnreadable { path });
        }
        crate::app::observe::PriorState::Present(_) => {
            return Decision::Refuse(Refusal::RecordConflict { path });
        }
    }
    Decision::Execute { plan, note }
}

/// 新 GPT 分区的唯一 GUID 由目标身份与区间稳定派生。
fn derive_unique(identity: diskedit_core::fingerprint::Fingerprint, start: u64, end: u64) -> Guid {
    let mut bytes = [0u8; 16];
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in identity
        .bytes()
        .iter()
        .copied()
        .chain(start.to_le_bytes())
        .chain(end.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for (index, chunk) in bytes.chunks_mut(8).enumerate() {
        let seed = hash.wrapping_add((index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        chunk.copy_from_slice(&seed.to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Guid(bytes)
}

/// 判定 `add`。
pub fn add(snapshot: &EditSnapshot, request: &Request) -> Decision {
    let Command::Add(add) = request.command() else {
        return refuse_edit("add reached with a different command");
    };
    if let Some(refusal) = precheck(snapshot, "add") {
        return Decision::Refuse(refusal);
    }
    let Some(identity) = snapshot.target.identity else {
        return Decision::Refuse(Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        });
    };
    if add.size == 0 {
        return Decision::Refuse(Refusal::ArgumentOutOfRange {
            what: "`add` needs a SIZE above zero".to_owned(),
        });
    }
    let Some(facts) = &snapshot.facts else {
        return Decision::Refuse(unusable(&snapshot.view));
    };
    match facts {
        TableFacts::Gpt {
            header,
            entries,
            sector_size,
            total_bytes,
        } => {
            let sectors = add.size.div_ceil(*sector_size);
            if sectors == 0 {
                return Decision::Refuse(Refusal::ArgumentOutOfRange {
                    what: "the requested size rounds down to zero sectors".to_owned(),
                });
            }
            let table = GptTable {
                header: header.clone(),
                entries: entries.clone(),
                sector_size: *sector_size,
                total_bytes: *total_bytes,
            };
            let placed = match place(
                &table.used_ranges(),
                header.first_usable_lba,
                header.last_usable_lba,
                add.align,
                *sector_size,
                sectors,
                add.start,
            ) {
                Ok(range) => range,
                Err(error) => return refuse_edit(&error.to_string()),
            };
            let type_guid = match add.type_guid {
                None => LINUX_FS_TYPE_GUID,
                Some(TypeSelector::Guid(guid)) => guid,
                Some(TypeSelector::Mbr(_)) => {
                    return refuse_edit(
                        "`--type-guid 0xNN` is an MBR type byte — a GPT entry takes a GUID",
                    );
                }
            };
            if type_guid.is_zero() {
                return refuse_edit(&EditError::ZeroTypeGuid.to_string());
            }
            let name = add.name.as_deref().unwrap_or("");
            if let Err(refusal) = check_name(name) {
                return Decision::Refuse(refusal);
            }
            let index = match table.add_slot(add.part_number, placed.0, placed.1) {
                Ok(index) => index,
                Err(error) => return refuse_edit(&error.to_string()),
            };
            let mut entries = table.entries.clone();
            entries[index] = edit::new_entry(
                placed.0,
                placed.1,
                type_guid,
                derive_unique(identity, placed.0, placed.1),
                name,
            );
            let segments = match table.segments(&entries) {
                Ok(segments) => segments,
                Err(error) => {
                    return Decision::Refuse(Refusal::TableDamaged {
                        why: error.to_string(),
                    });
                }
            };
            let number = u32::try_from(index + 1).expect("条目下标可表示");
            finish(
                snapshot,
                table_plan(
                    TableFormat::Gpt,
                    *sector_size,
                    segments,
                    snapshot.stamp,
                    identity,
                ),
                format!(
                    "added partition #{number} (verify with: diskedit info {})",
                    snapshot.target.display
                ),
            )
        }
        TableFacts::Mbr { lba0, total_lba } => {
            if add.name.is_some() {
                return Decision::Refuse(Refusal::GptRequired {
                    what: "`--name` sets a GPT partition name — MBR entries have none; drop --name"
                        .to_owned(),
                });
            }
            let os_type = match add.type_guid {
                None => LINUX_MBR_TYPE,
                Some(TypeSelector::Mbr(byte)) => byte,
                Some(TypeSelector::Guid(_)) => {
                    return refuse_edit(
                        "`--type-guid` with a GUID is a GPT type — an MBR entry takes a type byte like 0x83",
                    );
                }
            };
            let sectors = add.size.div_ceil(snapshot.target.sector_size);
            if sectors == 0 {
                return Decision::Refuse(Refusal::ArgumentOutOfRange {
                    what: "the requested size rounds down to zero sectors".to_owned(),
                });
            }
            let used = match edit::mbr_used_ranges(lba0, *total_lba) {
                Ok(used) => used,
                Err(error) => return refuse_edit(&error.to_string()),
            };
            let placed = match place(
                &used,
                1,
                total_lba.saturating_sub(1),
                add.align,
                snapshot.target.sector_size,
                sectors,
                add.start,
            ) {
                Ok(range) => range,
                Err(error) => return refuse_edit(&error.to_string()),
            };
            let (bytes, number) = match edit::add_mbr(
                lba0,
                add.part_number,
                placed.0,
                placed.1,
                os_type,
                *total_lba,
            ) {
                Ok(result) => result,
                Err(error) => return refuse_edit(&error.to_string()),
            };
            let sector_size = bytes.len() as u64;
            let segments = vec![Segment {
                region: region(0, sector_size),
                bytes,
            }];
            finish(
                snapshot,
                table_plan(
                    TableFormat::Mbr,
                    sector_size,
                    segments,
                    snapshot.stamp,
                    identity,
                ),
                format!(
                    "added partition #{number} (verify with: diskedit info {})",
                    snapshot.target.display
                ),
            )
        }
    }
}

/// 判定 `delete`。
pub fn delete(snapshot: &EditSnapshot, request: &Request) -> Decision {
    let Command::Delete { partition } = request.command() else {
        return refuse_edit("delete reached with a different command");
    };
    if let Some(refusal) = precheck(snapshot, "delete") {
        return Decision::Refuse(refusal);
    }
    let Some(identity) = snapshot.target.identity else {
        return Decision::Refuse(Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        });
    };
    let Some(facts) = &snapshot.facts else {
        return Decision::Refuse(unusable(&snapshot.view));
    };
    match facts {
        TableFacts::Gpt {
            header,
            entries,
            sector_size,
            total_bytes,
        } => {
            let table = GptTable {
                header: header.clone(),
                entries: entries.clone(),
                sector_size: *sector_size,
                total_bytes: *total_bytes,
            };
            let Some(number) = resolve_gpt(&table, *partition) else {
                return refuse_edit("there is no partition to point `:last` at");
            };
            let index = match table.live_index(number) {
                Ok(index) => index,
                Err(error) => return refuse_edit(&error.to_string()),
            };
            let mut entries = table.entries.clone();
            entries[index] = PartitionEntry::EMPTY;
            let segments = match table.segments(&entries) {
                Ok(segments) => segments,
                Err(error) => {
                    return Decision::Refuse(Refusal::TableDamaged {
                        why: error.to_string(),
                    });
                }
            };
            finish(
                snapshot,
                table_plan(
                    TableFormat::Gpt,
                    *sector_size,
                    segments,
                    snapshot.stamp,
                    identity,
                ),
                format!(
                    "deleted partition #{number} (verify with: diskedit info {})",
                    snapshot.target.display
                ),
            )
        }
        TableFacts::Mbr { lba0, .. } => {
            let Some(number) = resolve_mbr(lba0, *partition) else {
                return refuse_edit("there is no partition to point `:last` at");
            };
            let bytes = match edit::delete_mbr(lba0, number) {
                Ok(bytes) => bytes,
                Err(error) => return refuse_edit(&error.to_string()),
            };
            let sector_size = lba0.len() as u64;
            let segments = vec![Segment {
                region: region(0, sector_size),
                bytes,
            }];
            finish(
                snapshot,
                table_plan(
                    TableFormat::Mbr,
                    sector_size,
                    segments,
                    snapshot.stamp,
                    identity,
                ),
                format!(
                    "deleted partition #{number} (verify with: diskedit info {})",
                    snapshot.target.display
                ),
            )
        }
    }
}

/// 判定 `set`。
pub fn set(snapshot: &EditSnapshot, request: &Request) -> Decision {
    let Command::Set(set) = request.command() else {
        return refuse_edit("set reached with a different command");
    };
    if set.random && set.key != SetKey::Uuid {
        return Decision::Refuse(Refusal::ArgumentOutOfRange {
            what: "`--random` only applies to `set uuid`".to_owned(),
        });
    }
    if set.key != SetKey::Flag && !set.state.is_empty() {
        return refuse_edit(&format!(
            "`set {}` takes no state argument — only `set flag` does",
            set.key.as_str()
        ));
    }
    match set.key {
        SetKey::Label if set.value.is_empty() => {
            return refuse_edit("`set label` needs a value");
        }
        SetKey::Uuid if set.value.is_empty() && !set.random => {
            return refuse_edit("`set uuid` needs a value or --random");
        }
        SetKey::Name | SetKey::Label | SetKey::Uuid | SetKey::Flag | SetKey::Type => {}
    }
    if let Some(refusal) = precheck(snapshot, "set") {
        return Decision::Refuse(refusal);
    }
    let Some(identity) = snapshot.target.identity else {
        return Decision::Refuse(Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        });
    };
    let Some(facts) = &snapshot.facts else {
        return Decision::Refuse(unusable(&snapshot.view));
    };
    match facts {
        TableFacts::Gpt {
            header,
            entries,
            sector_size,
            total_bytes,
        } => set_gpt(
            snapshot,
            set,
            identity,
            GptTable {
                header: header.clone(),
                entries: entries.clone(),
                sector_size: *sector_size,
                total_bytes: *total_bytes,
            },
        ),
        TableFacts::Mbr { lba0, .. } => set_mbr(snapshot, set, identity, lba0),
    }
}

fn set_gpt(
    snapshot: &EditSnapshot,
    set: &SetRequest,
    identity: diskedit_core::fingerprint::Fingerprint,
    table: GptTable,
) -> Decision {
    let Some(number) = resolve_gpt(&table, set.partition) else {
        return refuse_edit("there is no partition to point `:last` at");
    };
    let index = match table.live_index(number) {
        Ok(index) => index,
        Err(error) => return refuse_edit(&error.to_string()),
    };
    let mut entries = table.entries.clone();
    let note;
    match set.key {
        SetKey::Name => {
            if set.value.is_empty() {
                return refuse_edit("`set name` needs a name");
            }
            if let Err(refusal) = check_name(&set.value) {
                return Decision::Refuse(refusal);
            }
            entries[index].name =
                diskedit_core::table::gpt::entries::PartitionName::from_str_lossy(&set.value);
            note = format!("renamed partition #{number} to {:?}", set.value);
        }
        SetKey::Type => {
            if set.value.is_empty() {
                return refuse_edit("`set type` needs a type GUID");
            }
            let Some(guid) = Guid::parse(&set.value) else {
                return refuse_edit(&format!(
                    "`{}` is not a GUID — expected 8-4-4-4-12 hexadecimal",
                    set.value
                ));
            };
            if guid.is_zero() {
                return refuse_edit(&EditError::ZeroTypeGuid.to_string());
            }
            entries[index].partition_type_guid = guid;
            note = format!(
                "type set on partition #{number} (verify with: diskedit info {})",
                snapshot.target.display
            );
        }
        SetKey::Flag => {
            let (Some(flag), Some(on)) = (GptFlag::parse(&set.value), parse_state(&set.state))
            else {
                return refuse_edit(&format!(
                    "`set flag` takes one of {} and `on|off`",
                    GptFlag::NAMES
                ));
            };
            let fallback = match snapshot.view.find(number) {
                Some(partition) if matches!(partition.content.as_str(), "vfat" | "ntfs") => {
                    Some(BASIC_DATA_TYPE_GUID)
                }
                _ => Some(LINUX_FS_TYPE_GUID),
            };
            if let Err(error) = edit::apply_gpt_flag(&mut entries[index], flag, on, fallback) {
                return refuse_edit(&error.to_string());
            }
            note = format!("flag {}={} on partition #{number}", set.value, set.state);
        }
        SetKey::Label | SetKey::Uuid => {
            return filesystem_attr(snapshot, number, set.key);
        }
    }
    let segments = match table.segments(&entries) {
        Ok(segments) => segments,
        Err(error) => {
            return Decision::Refuse(Refusal::TableDamaged {
                why: error.to_string(),
            });
        }
    };
    finish(
        snapshot,
        table_plan(
            TableFormat::Gpt,
            table.sector_size,
            segments,
            snapshot.stamp,
            identity,
        ),
        note,
    )
}

fn set_mbr(
    snapshot: &EditSnapshot,
    set: &SetRequest,
    identity: diskedit_core::fingerprint::Fingerprint,
    lba0: &[u8],
) -> Decision {
    let Some(number) = resolve_mbr_number(lba0, set.partition) else {
        return refuse_edit("there is no partition to point `:last` at");
    };
    if set.key == SetKey::Name {
        return Decision::Refuse(Refusal::GptRequired {
            what: "`set name` sets a GPT partition name — MBR entries have none".to_owned(),
        });
    }
    if matches!(set.key, SetKey::Label | SetKey::Uuid) {
        return filesystem_attr(snapshot, number, set.key);
    }
    let note;
    let bytes = match set.key {
        SetKey::Type => {
            if set.value.is_empty() {
                return refuse_edit("`set type` needs a type byte like 0x83");
            }
            let Some(byte) = parse_mbr_type(&set.value) else {
                return refuse_edit(&format!(
                    "`{}` is not an MBR type byte — expected 0xNN",
                    set.value
                ));
            };
            match edit::set_mbr_type(lba0, number, byte) {
                Ok(bytes) => {
                    note = format!(
                        "type set on partition #{number} (verify with: diskedit info {})",
                        snapshot.target.display
                    );
                    bytes
                }
                Err(error) => return refuse_edit(&error.to_string()),
            }
        }
        SetKey::Flag => {
            let Some(flag) = MbrFlag::parse(&set.value) else {
                return refuse_edit(&format!(
                    "on an MBR table `set flag` takes one of {}",
                    MbrFlag::NAMES
                ));
            };
            let Some(on) = parse_state(&set.state) else {
                return refuse_edit("`set flag` takes `on|off`");
            };
            let bytes = match flag {
                MbrFlag::Boot => match edit::set_mbr_boot(lba0, number, on) {
                    Ok(bytes) => bytes,
                    Err(error) => return refuse_edit(&error.to_string()),
                },
                MbrFlag::Hidden => match edit::set_mbr_hidden(lba0, number, on) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        return Decision::Unchanged {
                            note: format!("flag hidden={} on partition #{number}", set.state),
                        };
                    }
                    Err(error) => return refuse_edit(&error.to_string()),
                },
            };
            note = format!("flag {}={} on partition #{number}", set.value, set.state);
            bytes
        }
        SetKey::Name | SetKey::Label | SetKey::Uuid => {
            unreachable!("上面的分派已经拦下这些属性")
        }
    };
    let segments = vec![Segment {
        region: region(0, lba0.len() as u64),
        bytes,
    }];
    finish(
        snapshot,
        table_plan(
            TableFormat::Mbr,
            lba0.len() as u64,
            segments,
            snapshot.stamp,
            identity,
        ),
        note,
    )
}

/// 自动/显式起点的统一入口。
fn place(
    used: &[(u64, u64)],
    first: u64,
    last: u64,
    alignment: Alignment,
    sector_size: u64,
    sectors: u64,
    start: Option<StartSpec>,
) -> Result<(u64, u64), EditError> {
    match start {
        None => edit::first_fit(
            used,
            first,
            last,
            auto_unit(alignment, sector_size),
            sectors,
        ),
        Some(StartSpec::Lba(lba)) => edit::fixed_start(
            used,
            first,
            last,
            start_unit(alignment, sector_size),
            sectors,
            lba,
        ),
        Some(StartSpec::End) => Err(EditError::UnusableTable(
            "`--start end` is not valid for `add` — give an LBA or omit --start",
        )),
        Some(StartSpec::Align) => Err(EditError::UnusableTable(
            "`--start align` is not valid for `add` — use --align",
        )),
    }
}

/// 自动放置的对齐单位：未给出 `--align` 时按 1 MiB。
fn auto_unit(alignment: Alignment, sector_size: u64) -> u64 {
    match alignment {
        Alignment::Default | Alignment::Mib => (1 << 20) / sector_size.max(1),
        Alignment::None => 1,
    }
}

/// 显式起点的对齐单位：未给出 `--align` 时按给定 LBA 原样落位。
fn start_unit(alignment: Alignment, sector_size: u64) -> u64 {
    match alignment {
        Alignment::Default | Alignment::None => 1,
        Alignment::Mib => (1 << 20) / sector_size.max(1),
    }
}

fn resolve_gpt(table: &GptTable, selector: PartitionSelector) -> Option<u32> {
    match selector {
        PartitionSelector::Number(number) => Some(number),
        PartitionSelector::Last => table.last_live(),
    }
}

fn resolve_mbr(lba0: &[u8], selector: PartitionSelector) -> Option<u32> {
    resolve_mbr_number(lba0, selector)
}

fn resolve_mbr_number(lba0: &[u8], selector: PartitionSelector) -> Option<u32> {
    match selector {
        PartitionSelector::Number(number) => Some(number),
        PartitionSelector::Last => edit::mbr_last_live(lba0).ok().flatten(),
    }
}

fn check_name(name: &str) -> Result<(), Refusal> {
    let units = name.encode_utf16().count();
    if units > 36 {
        return Err(Refusal::TableEdit {
            why: format!(
                "a GPT partition name holds at most 36 UTF-16 units; `{name}` has {units}"
            ),
        });
    }
    Ok(())
}

fn parse_state(state: &str) -> Option<bool> {
    match state {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn parse_mbr_type(value: &str) -> Option<u8> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u8::from_str_radix(digits, 16).ok()
}

fn content_unsupported(key: SetKey) -> Refusal {
    Refusal::ContentUnsupported {
        what: format!("set {}", key.as_str()),
        why: "filesystem label and UUID writes run through the filesystem execution path, which this build does not wire yet".to_owned(),
    }
}

/// 文件系统属性：先要求分区承载可识别的内容，再说明本构建未接入写路径。
fn filesystem_attr(snapshot: &EditSnapshot, number: u32, key: SetKey) -> Decision {
    match snapshot.view.find(number) {
        Some(partition) if partition.content.as_str() != "unknown" => {
            Decision::Refuse(content_unsupported(key))
        }
        _ => Decision::Refuse(Refusal::FilesystemUnknown { number }),
    }
}

/// 公共前置：分区节点、表形态、占用。
fn precheck(snapshot: &EditSnapshot, command: &str) -> Option<Refusal> {
    if snapshot.is_partition_node {
        return Some(Refusal::PartitionNodeTarget {
            command: command.to_owned(),
            target: snapshot.target.display.clone(),
        });
    }
    match &snapshot.view {
        TableView::None => return Some(Refusal::TableMissing),
        TableView::Damaged { why, .. } => {
            return Some(Refusal::TableDamaged { why: why.clone() });
        }
        TableView::Gpt(_) | TableView::Mbr(_) => {}
    }
    match &snapshot.occupancy {
        crate::platform::Occupancy::Idle => None,
        crate::platform::Occupancy::Used { evidence } => Some(Refusal::Occupied {
            evidence: evidence.clone(),
        }),
        crate::platform::Occupancy::Unknown { why } => {
            Some(Refusal::OccupancyUnknown { why: why.clone() })
        }
    }
}

fn unusable(view: &TableView) -> Refusal {
    match view {
        TableView::None => Refusal::TableMissing,
        TableView::Damaged { why, .. } => Refusal::TableDamaged { why: why.clone() },
        TableView::Gpt(_) | TableView::Mbr(_) => Refusal::TableEdit {
            why: "the table bytes could not be read back for editing".to_owned(),
        },
    }
}

fn refuse_edit(why: &str) -> Decision {
    Decision::Refuse(Refusal::TableEdit {
        why: why.to_owned(),
    })
}

/// 组装执行计划，并处理未收尾记录的同计划续跑/异计划冲突。
fn finish(snapshot: &EditSnapshot, plan: Result<Plan, Refusal>, note: String) -> Decision {
    let plan = match plan {
        Ok(plan) => plan,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let path = record_path(snapshot);
    match &snapshot.prior {
        crate::app::observe::PriorState::Absent => {}
        crate::app::observe::PriorState::Unreadable => {
            return Decision::Refuse(Refusal::RecordUnreadable { path });
        }
        crate::app::observe::PriorState::Present(prior) if prior.pending => {
            if prior.plan_digest != plan.digest() {
                return Decision::Refuse(Refusal::RecordConflict { path });
            }
        }
        crate::app::observe::PriorState::Present(_) => {}
    }
    Decision::Execute { plan, note }
}

fn record_path(snapshot: &EditSnapshot) -> String {
    snapshot
        .record
        .file
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(record path unresolved)".to_owned())
}

fn table_plan(
    format: TableFormat,
    segment_size: u64,
    segments: Vec<Segment>,
    stamp: [u8; 32],
    identity: diskedit_core::fingerprint::Fingerprint,
) -> Result<Plan, Refusal> {
    let regions = segments.iter().map(|segment| segment.region).collect();
    let segments: Vec<TableSegment> = segments
        .into_iter()
        .map(|segment| TableSegment {
            region: segment.region,
            bytes: segment.bytes,
        })
        .collect();
    let step = Step::new(
        StepKind::WriteTable {
            format,
            segment_size,
            stamp,
            segments,
        },
        regions,
    );
    Plan::write(vec![step], identity)
}

fn region(start: u64, len: u64) -> diskedit_core::region::Region {
    diskedit_core::region::Region::sized(start, len).expect("由当前盘面导出的区间自洽")
}
