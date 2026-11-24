//! 分区表步骤：读出现表、改条目、写回主备副本。
//!
//! 所有落盘字节都由本模块拼好后经写通道写出；区间来自步骤声明，通道再校验
//! 一次覆盖关系。GPT 同时重写主头、主数组、备数组、备头与保护 MBR。

use diskedit_core::region::Region;
use diskedit_core::table::gpt::entries::{serialize_entries, validate_entries};
use diskedit_core::table::gpt::header::GptHeader;
use diskedit_core::table::gpt::parse::{ParsedCopy, parse_backup, parse_primary};
use diskedit_core::table::gpt::write::array_crc;
use diskedit_core::table::mbr::primary::{Mbr, parse as parse_mbr, protective_mbr, write_into};
use diskedit_core::table::{ENTRY_MIN_BYTES, GptCopy, Guid, MAX_ENTRIES, MbrEntry, PartitionEntry};

use super::super::token::Recorded;
use super::super::{ExecError, WriteChannel};
use super::Context;
use super::entries;
use crate::app::decide::step::StepKind;
use crate::app::observe::{SetProperty, TableFormat};

/// 条目数组的标准字节数。
const ARRAY_BYTES: usize = MAX_ENTRIES as usize * ENTRY_MIN_BYTES;
/// 保护 MBR 的字节数。
const MBR_BYTES: usize = 512;

/// 现表内容。
enum Current {
    Gpt {
        header: GptHeader,
        entries: Vec<PartitionEntry>,
    },
    Mbr {
        mbr: Mbr,
        sector: Box<[u8; MBR_BYTES]>,
    },
    None,
}

/// 执行一个分区表步骤；非表步骤由调用方分派，不会到这里。
pub fn apply(
    kind: &StepKind,
    channel: &mut WriteChannel<'_>,
    token: &Recorded,
    ctx: &Context<'_>,
) -> Result<(), ExecError> {
    match kind {
        StepKind::WriteTable { format } => match format {
            TableFormat::Gpt => write_fresh_gpt(channel, token, ctx),
            TableFormat::Mbr => write_fresh_mbr(channel, token),
        },
        StepKind::AddPartition {
            format,
            start_lba,
            sectors,
            type_guid,
            name,
            part_number,
            unique,
        } => with_table(channel, ctx, token, *format, |current| match current {
            Current::Gpt {
                header,
                mut entries,
            } => {
                let unique =
                    unique.unwrap_or_else(|| entries::guid_from_seed(ctx.plan_digest, *start_lba));
                entries::add_gpt(
                    &mut entries,
                    *start_lba,
                    *sectors,
                    *type_guid,
                    name,
                    unique,
                    *part_number,
                )
                .map_err(ExecError::InvalidPlan)?;
                Ok(Current::Gpt { header, entries })
            }
            Current::Mbr { mbr, sector } => {
                let mut mbr = mbr;
                let part_type = match type_guid.0[0] {
                    0 => 0x83,
                    byte => byte,
                };
                entries::add_mbr(&mut mbr, *start_lba, *sectors, part_type, *part_number)
                    .map_err(ExecError::InvalidPlan)?;
                Ok(Current::Mbr { mbr, sector })
            }
            Current::None => Err(ExecError::InvalidPlan(
                "adding a partition requires an existing partition table".to_owned(),
            )),
        }),
        StepKind::DeletePartition { format, partition } => {
            with_table(channel, ctx, token, *format, |current| match current {
                Current::Gpt {
                    header,
                    mut entries,
                } => {
                    entries::delete_gpt(&mut entries, *partition)
                        .map_err(ExecError::InvalidPlan)?;
                    Ok(Current::Gpt { header, entries })
                }
                Current::Mbr { mbr, sector } => {
                    let mut mbr = mbr;
                    entries::delete_mbr(&mut mbr, *partition).map_err(ExecError::InvalidPlan)?;
                    Ok(Current::Mbr { mbr, sector })
                }
                Current::None => Err(ExecError::InvalidPlan(
                    "no partition table to delete".to_owned(),
                )),
            })
        }
        StepKind::ResizePartition {
            format,
            partition,
            start_lba,
            sectors,
        } => with_table(channel, ctx, token, *format, |current| match current {
            Current::Gpt {
                header,
                mut entries,
            } => {
                entries::resize_gpt(&mut entries, *partition, *start_lba, *sectors)
                    .map_err(ExecError::InvalidPlan)?;
                Ok(Current::Gpt { header, entries })
            }
            Current::Mbr { mbr, sector } => {
                let mut mbr = mbr;
                entries::resize_mbr(&mut mbr, *partition, *start_lba, *sectors)
                    .map_err(ExecError::InvalidPlan)?;
                Ok(Current::Mbr { mbr, sector })
            }
            Current::None => Err(ExecError::InvalidPlan(
                "no partition table to resize".to_owned(),
            )),
        }),
        StepKind::SetProperty {
            partition,
            property: property @ (SetProperty::Name | SetProperty::Flag | SetProperty::Type),
            value,
            ..
        } => {
            let format = table_format(channel, ctx)?;
            with_table(channel, ctx, token, format, |current| match current {
                Current::Gpt {
                    header,
                    mut entries,
                } => {
                    apply_gpt_property(&mut entries, *partition, *property, value)?;
                    Ok(Current::Gpt { header, entries })
                }
                Current::Mbr { .. } => Err(ExecError::Unsupported(
                    "unsupported partition property for MBR".to_owned(),
                )),
                Current::None => Err(ExecError::InvalidPlan(
                    "no partition table to modify".to_owned(),
                )),
            })
        }
        StepKind::SetProperty {
            property: SetProperty::Label | SetProperty::Uuid,
            ..
        }
        | StepKind::MakeFilesystem { .. }
        | StepKind::GrowFilesystem { .. }
        | StepKind::ShrinkFilesystem { .. }
        | StepKind::CopyPartition { .. }
        | StepKind::MovePartition { .. }
        | StepKind::Inspect
        | StepKind::Check { .. }
        | StepKind::ListRecords
        | StepKind::VerifyRecord { .. }
        | StepKind::ScanRecords
        | StepKind::RefreshKernelView
        | StepKind::RecoverRecords { .. } => Err(ExecError::InvalidPlan(
            "not a partition table step".to_owned(),
        )),
    }
}

fn apply_gpt_property(
    entries: &mut [PartitionEntry],
    partition: u32,
    property: SetProperty,
    value: &str,
) -> Result<(), ExecError> {
    match property {
        SetProperty::Name => {
            entries::set_name(entries, partition, value).map_err(ExecError::InvalidPlan)
        }
        SetProperty::Flag => {
            let (flag, on) = parse_flag(value)?;
            entries::set_attribute_bit(entries, partition, flag, on).map_err(ExecError::InvalidPlan)
        }
        SetProperty::Type => {
            let guid = parse_guid(value)?;
            entries::set_type(entries, partition, guid).map_err(ExecError::InvalidPlan)
        }
        SetProperty::Label | SetProperty::Uuid => Err(ExecError::InvalidPlan(
            "this property is not a partition table property".to_owned(),
        )),
    }
}

/// 解析 `F:on` / `F:off`，返回属性位与目标状态。
fn parse_flag(value: &str) -> Result<(u32, bool), ExecError> {
    let (flag, state) = value.split_once(':').ok_or_else(|| {
        ExecError::InvalidPlan(format!("property {value} is missing its on/off state"))
    })?;
    let flag = flag
        .trim()
        .parse::<u32>()
        .map_err(|_| ExecError::InvalidPlan(format!("property bit {flag} is not an integer")))?;
    let on = match state.trim() {
        "on" => true,
        "off" => false,
        other => {
            return Err(ExecError::InvalidPlan(format!(
                "property state {other} must be on or off"
            )));
        }
    };
    Ok((flag, on))
}

fn parse_guid(value: &str) -> Result<Guid, ExecError> {
    let mut bytes = [0u8; 16];
    let hex: String = value.chars().filter(|ch| *ch != '-').collect();
    if hex.len() != 32 {
        return Err(ExecError::InvalidPlan(format!(
            "GUID {value} has invalid length"
        )));
    }
    for (index, slot) in bytes.iter_mut().enumerate() {
        let pair = hex
            .get(index * 2..index * 2 + 2)
            .ok_or_else(|| ExecError::InvalidPlan(format!("GUID {value} has invalid length")))?;
        *slot = u8::from_str_radix(pair, 16).map_err(|_| {
            ExecError::InvalidPlan(format!("GUID {value} contains non-hex characters"))
        })?;
    }
    Ok(Guid(bytes))
}

fn with_table(
    channel: &mut WriteChannel<'_>,
    ctx: &Context<'_>,
    token: &Recorded,
    format: TableFormat,
    edit: impl FnOnce(Current) -> Result<Current, ExecError>,
) -> Result<(), ExecError> {
    let current = read_current(channel, ctx.sector)?;
    let edited = edit(current)?;
    match edited {
        Current::Gpt { header, entries } => {
            if format != TableFormat::Gpt {
                return Err(ExecError::InvalidPlan(
                    "table format mismatch with step declaration".to_owned(),
                ));
            }
            write_gpt(channel, token, ctx, header, entries)
        }
        Current::Mbr { mbr, sector } => {
            if format != TableFormat::Mbr {
                return Err(ExecError::InvalidPlan(
                    "table format mismatch with step declaration".to_owned(),
                ));
            }
            let mut sector = sector;
            write_into(&mbr, &mut sector);
            let region = Region::sized(0, MBR_BYTES as u64)
                .map_err(|error| ExecError::InvalidPlan(format!("empty region: {error:?}")))?;
            channel.write(token, region, &*sector)
        }
        Current::None => Err(ExecError::InvalidPlan(
            "partition table not found".to_owned(),
        )),
    }
}

fn table_format(
    channel: &mut WriteChannel<'_>,
    ctx: &Context<'_>,
) -> Result<TableFormat, ExecError> {
    match read_current(channel, ctx.sector)? {
        Current::Gpt { .. } => Ok(TableFormat::Gpt),
        Current::Mbr { .. } => Ok(TableFormat::Mbr),
        Current::None => Err(ExecError::InvalidPlan(
            "partition table not found".to_owned(),
        )),
    }
}

fn read_current(channel: &mut WriteChannel<'_>, sector: u64) -> Result<Current, ExecError> {
    let mbr_raw = channel.read_exact(0, MBR_BYTES)?;
    if let Ok(mbr) = parse_mbr(&mbr_raw) {
        let protective = mbr.entries.iter().any(|entry| entry.part_type == 0xEE);
        if !protective && mbr.entries.iter().any(|entry| !entry.is_empty()) {
            let mut raw = Box::new([0u8; MBR_BYTES]);
            raw.copy_from_slice(&mbr_raw);
            return Ok(Current::Mbr { mbr, sector: raw });
        }
    }
    if let Some(copy) = read_primary_gpt(channel, sector)? {
        return Ok(Current::Gpt {
            header: copy.header,
            entries: copy.entries,
        });
    }
    if let Some(copy) = read_backup_gpt(channel, sector)? {
        return Ok(Current::Gpt {
            header: copy.header,
            entries: copy.entries,
        });
    }
    if let Ok(mbr) = parse_mbr(&mbr_raw) {
        let mut raw = Box::new([0u8; MBR_BYTES]);
        raw.copy_from_slice(&mbr_raw);
        return Ok(Current::Mbr { mbr, sector: raw });
    }
    Ok(Current::None)
}

fn read_primary_gpt(
    channel: &mut WriteChannel<'_>,
    sector: u64,
) -> Result<Option<GptCopy>, ExecError> {
    let last_lba = last_lba(channel, sector)?;
    let sector_usize = usize::try_from(sector)
        .map_err(|_| ExecError::InvalidPlan("sector size exceeds addressable range".to_owned()))?;
    let header = channel.read_exact(sector, sector_usize)?;
    let array = channel.read_exact(2 * sector, ARRAY_BYTES)?;
    match parse_primary(&header, &array, last_lba, sector) {
        ParsedCopy::Usable(copy) => Ok(Some(copy)),
        ParsedCopy::Absent | ParsedCopy::Damaged(_) => Ok(None),
    }
}

fn read_backup_gpt(
    channel: &mut WriteChannel<'_>,
    sector: u64,
) -> Result<Option<GptCopy>, ExecError> {
    let last_lba = last_lba(channel, sector)?;
    let array_span = (ARRAY_BYTES as u64).div_ceil(sector);
    let array_lba = last_lba.saturating_sub(array_span);
    let sector_usize = usize::try_from(sector)
        .map_err(|_| ExecError::InvalidPlan("sector size exceeds addressable range".to_owned()))?;
    let header = channel.read_exact(last_lba * sector, sector_usize)?;
    let array = channel.read_exact(array_lba * sector, ARRAY_BYTES)?;
    match parse_backup(&header, &array, last_lba, sector) {
        ParsedCopy::Usable(copy) => Ok(Some(copy)),
        ParsedCopy::Absent | ParsedCopy::Damaged(_) => Ok(None),
    }
}

fn write_fresh_gpt(
    channel: &mut WriteChannel<'_>,
    token: &Recorded,
    ctx: &Context<'_>,
) -> Result<(), ExecError> {
    let sector = ctx.sector;
    let last_lba = last_lba(channel, sector)?;
    let array_span = (ARRAY_BYTES as u64).div_ceil(sector);
    let header = GptHeader {
        revision: 0x0001_0000,
        primary_lba: 1,
        backup_lba: last_lba,
        first_usable_lba: 2 + array_span,
        last_usable_lba: last_lba
            .checked_sub(array_span + 1)
            .ok_or_else(|| ExecError::InvalidPlan("target too small for GPT".to_owned()))?,
        disk_guid: entries::guid_from_seed(ctx.plan_digest, 0),
        partition_entry_lba: 2,
        number_of_partition_entries: MAX_ENTRIES,
        size_of_partition_entry: ENTRY_MIN_BYTES as u32,
        header_size: 92,
        entry_array_crc32: 0,
    };
    let entries = vec![entries::EMPTY_GPT; MAX_ENTRIES as usize];
    write_gpt(channel, token, ctx, header, entries)
}

fn write_gpt(
    channel: &mut WriteChannel<'_>,
    token: &Recorded,
    ctx: &Context<'_>,
    header: GptHeader,
    entries: Vec<PartitionEntry>,
) -> Result<(), ExecError> {
    let sector = ctx.sector;
    let last_lba = last_lba(channel, sector)?;
    validate_entries(&entries, header.first_usable_lba, header.last_usable_lba)
        .map_err(|error| ExecError::InvalidPlan(format!("invalid partition entry: {error:?}")))?;
    let sector_usize = usize::try_from(sector)
        .map_err(|_| ExecError::InvalidPlan("sector size exceeds addressable range".to_owned()))?;
    let array = serialize_entries(
        &entries,
        header.number_of_partition_entries,
        header.size_of_partition_entry,
        sector_usize,
    )
    .map_err(|error| {
        ExecError::InvalidPlan(format!("failed to serialize entry array: {error:?}"))
    })?;
    let crc = array_crc(&array);
    let array_span = (array.len() as u64).div_ceil(sector);
    let backup_array_lba = last_lba.checked_sub(array_span).ok_or_else(|| {
        ExecError::InvalidPlan("target too small for the backup array".to_owned())
    })?;

    let mut primary = header;
    primary.primary_lba = 1;
    primary.backup_lba = last_lba;
    primary.partition_entry_lba = 2;
    primary.entry_array_crc32 = crc;
    let primary_bytes = primary.serialize(sector_usize).map_err(|error| {
        ExecError::InvalidPlan(format!("failed to serialize primary header: {error:?}"))
    })?;

    let mut backup = primary.clone();
    backup.primary_lba = last_lba;
    backup.backup_lba = 1;
    backup.partition_entry_lba = backup_array_lba;
    let backup_bytes = backup.serialize(sector_usize).map_err(|error| {
        ExecError::InvalidPlan(format!("failed to serialize backup header: {error:?}"))
    })?;

    let mbr = protective_mbr(last_lba + 1);
    let mbr_region = region_of(0, MBR_BYTES as u64)?;
    let primary_header_region = region_of(sector, sector)?;
    let primary_array_region = region_of(2 * sector, array.len() as u64)?;
    let backup_array_region = region_of(backup_array_lba * sector, array.len() as u64)?;
    let backup_header_region = region_of(last_lba * sector, sector)?;

    channel.write(token, mbr_region, &mbr)?;
    channel.write(token, primary_header_region, &primary_bytes)?;
    channel.write(token, primary_array_region, &array)?;
    channel.write(token, backup_array_region, &array)?;
    channel.write(token, backup_header_region, &backup_bytes)?;
    Ok(())
}

fn write_fresh_mbr(channel: &mut WriteChannel<'_>, token: &Recorded) -> Result<(), ExecError> {
    let mbr = Mbr {
        entries: [MbrEntry::default(); 4],
    };
    let mut sector = [0u8; MBR_BYTES];
    write_into(&mbr, &mut sector);
    let region = region_of(0, MBR_BYTES as u64)?;
    channel.write(token, region, &sector)
}

/// 按 1 起的分区号取起始 LBA 与扇区数。
pub fn partition_span(
    channel: &mut WriteChannel<'_>,
    sector: u64,
    partition: u32,
) -> Result<(u64, u64), ExecError> {
    match read_current(channel, sector)? {
        Current::Gpt { entries, .. } => {
            let index = entries::index_of(partition).map_err(ExecError::InvalidPlan)?;
            let entry = entries
                .get(index)
                .filter(|entry| !entry.partition_type_guid.is_zero())
                .ok_or_else(|| {
                    ExecError::InvalidPlan(format!("partition {partition} not found"))
                })?;
            Ok((
                entry.starting_lba,
                entry.ending_lba - entry.starting_lba + 1,
            ))
        }
        Current::Mbr { mbr, .. } => {
            let index = entries::index_of(partition).map_err(ExecError::InvalidPlan)?;
            let entry = mbr
                .entries
                .get(index)
                .filter(|entry| !entry.is_empty())
                .ok_or_else(|| {
                    ExecError::InvalidPlan(format!("partition {partition} not found"))
                })?;
            Ok((u64::from(entry.start_lba), u64::from(entry.size_lba)))
        }
        Current::None => Err(ExecError::InvalidPlan(
            "partition table not found".to_owned(),
        )),
    }
}

fn region_of(start: u64, len: u64) -> Result<Region, ExecError> {
    Region::sized(start, len)
        .map_err(|error| ExecError::InvalidPlan(format!("empty region: {error:?}")))
}

/// 目标最后 LBA。
fn last_lba(channel: &WriteChannel<'_>, sector: u64) -> Result<u64, ExecError> {
    if sector == 0 {
        return Err(ExecError::InvalidPlan(
            "sector size cannot be zero".to_owned(),
        ));
    }
    let len = channel.target_len();
    if len < sector {
        return Err(ExecError::InvalidPlan(
            "target is smaller than one sector".to_owned(),
        ));
    }
    Ok(len / sector - 1)
}
