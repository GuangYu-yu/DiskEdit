//! `set`：修改分区表属性或文件系统属性。
//!
//! 表属性只改分区表；标签与 UUID 走内容档案，需要对应程序可用。

use diskedit_core::fs::{ContentKind, FsKind, Verb};

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::snapshot::PartitionFacts;
use crate::app::observe::{PartitionSelector, SetAction, SetProperty, Snapshot, TableFormat};

pub fn plan(
    snapshot: &Snapshot,
    selector: PartitionSelector,
    action: &SetAction,
) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let partition = admission::select(snapshot, selector)?;
    let facts = admission::partition(snapshot, partition)?;
    let (property, value) = encode(action);
    match property {
        SetProperty::Name | SetProperty::Flag | SetProperty::Type => {
            let table = admission::table(snapshot)?;
            let format = table.format();
            if format == TableFormat::Mbr && property == SetProperty::Name {
                return Err(Refusal::ContentUnsupported {
                    what: "MBR partition name".to_owned(),
                    why: "MBR partitions have no name".to_owned(),
                });
            }
            let exists = match table {
                admission::TableView::Gpt(copy) => {
                    admission::gpt_used(copy).any(|(n, _)| n == partition)
                }
                admission::TableView::Mbr(mbr) => {
                    admission::mbr_used(mbr).any(|(n, _)| n == partition)
                }
            };
            if !exists {
                return Err(Refusal::PartitionMissing { number: partition });
            }
            let layout = match format {
                TableFormat::Gpt => Some(capacity::gpt_layout(admitted.length, admitted.sector)?),
                TableFormat::Mbr => None,
            };
            let regions = capacity::table_regions(format, layout, admitted.sector)?;
            let value = match action {
                SetAction::Flag { flag, on } => resolve_flag(format, flag, *on, &table, facts)?,
                SetAction::Name(_)
                | SetAction::Label(_)
                | SetAction::Uuid { .. }
                | SetAction::Type(_) => value,
            };
            let steps = vec![Step::new(
                StepKind::SetProperty {
                    partition,
                    property,
                    value,
                    content: None,
                },
                regions,
            )];
            Plan::write(steps, admitted.identity)
        }
        SetProperty::Label | SetProperty::Uuid => {
            let content = admission::content(facts)?;
            let programs = admission::programs(content, Verb::Attrs)?;
            let Some(programs) = programs else {
                return Err(Refusal::ContentUnsupported {
                    what: format!("{} properties", content.as_str()),
                    why: "this content has no modifiable properties".to_owned(),
                });
            };
            admission::require_tools(snapshot, programs)?;
            let region = capacity::partition_region(
                facts.partition.start_lba,
                facts.partition.sectors,
                admitted.sector,
            )?;
            let steps = vec![Step::new(
                StepKind::SetProperty {
                    partition,
                    property,
                    value,
                    content: Some(content),
                },
                vec![region],
            )];
            Plan::write(steps, admitted.identity)
        }
    }
}

/// 把带类型的属性动作折成属性与值。
///
/// 值在计划摘要里按字节参与比对，执行层再按属性种类解析。
fn encode(action: &SetAction) -> (SetProperty, String) {
    match action {
        SetAction::Name(value) => (SetProperty::Name, value.clone()),
        SetAction::Label(value) => (SetProperty::Label, value.clone()),
        SetAction::Uuid { value, random } => {
            let text = value.clone().unwrap_or_else(|| {
                if *random {
                    "random".to_owned()
                } else {
                    String::new()
                }
            });
            (SetProperty::Uuid, text)
        }
        SetAction::Flag { flag, on } => (
            SetProperty::Flag,
            format!("{flag}:{}", if *on { "on" } else { "off" }),
        ),
        SetAction::Type(value) => (SetProperty::Type, value.clone()),
    }
}

/// Linux 文件系统数据分区（非 ESP 时的回落类型）。
const GUID_LINUX_DATA: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";
/// 微软基本数据分区（FAT/NTFS 内容的回落类型）。
const GUID_MICROSOFT_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
/// EFI 系统分区。
const GUID_ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

/// 把旗标名折成执行层能直接落盘的指令。
///
/// 名字集合与语义对齐基线：GPT 的 `esp`/`boot` 切换类型 GUID，
/// `legacy`/`legacy_boot`、`hidden`、`required` 改属性位；MBR 只有
/// `boot`（状态字节）与 `hidden`（类型字节 ±0x10）。未知名字按事实类别拒绝。
fn resolve_flag(
    format: TableFormat,
    flag: &str,
    on: bool,
    table: &admission::TableView<'_>,
    facts: &PartitionFacts,
) -> Result<String, Refusal> {
    let state = if on { "on" } else { "off" };
    match format {
        TableFormat::Gpt => match flag {
            "esp" | "boot" => {
                if on {
                    Ok(format!("esp:on:{GUID_ESP}"))
                } else {
                    let microsoft = matches!(
                        admission::content(facts),
                        Ok(ContentKind::Filesystem(FsKind::Ntfs | FsKind::Vfat))
                    );
                    let fallback = if microsoft {
                        GUID_MICROSOFT_DATA
                    } else {
                        GUID_LINUX_DATA
                    };
                    Ok(format!("esp:off:{fallback}"))
                }
            }
            "legacy" | "legacy_boot" => Ok(format!("bit:2:{state}")),
            "hidden" => Ok(format!("bit:1:{state}")),
            "required" => Ok(format!("bit:0:{state}")),
            other => Err(Refusal::FlagUnknown {
                why: format!(
                    "unknown gpt flag {other} (accepted: esp/boot, legacy/legacy_boot, hidden, required)"
                ),
            }),
        },
        TableFormat::Mbr => match flag {
            "boot" => Ok(format!("mbr-boot:{state}")),
            "hidden" => {
                let part_type = match table {
                    admission::TableView::Mbr(mbr) => admission::mbr_used(mbr)
                        .find(|(number, _)| *number == facts.partition.number)
                        .map(|(_, entry)| entry.part_type)
                        .unwrap_or_default(),
                    admission::TableView::Gpt(_) => 0,
                };
                let mapped = if on {
                    hidden_counterpart(part_type)
                } else {
                    visible_counterpart(part_type)
                };
                mapped.map(|kind| format!("mbr-type:0x{kind:02x}")).ok_or_else(|| {
                    Refusal::FlagUnknown {
                        why: format!(
                            "type 0x{part_type:02x} has no hidden counterpart (supported: fat12/16/32 incl. lba, ntfs)"
                        ),
                    }
                })
            }
            other => Err(Refusal::FlagUnknown {
                why: format!("unknown mbr flag {other} (accepted: boot, hidden)"),
            }),
        },
    }
}

/// FAT/NTFS 可见类型到隐藏类型的映射；其它类型没有隐藏形态。
fn hidden_counterpart(part_type: u8) -> Option<u8> {
    matches!(part_type, 0x01 | 0x04 | 0x06 | 0x0b | 0x0c | 0x07).then_some(part_type + 0x10)
}

/// 隐藏类型回到可见类型。
fn visible_counterpart(part_type: u8) -> Option<u8> {
    matches!(part_type, 0x11 | 0x14 | 0x16 | 0x1b | 0x1c | 0x17).then_some(part_type - 0x10)
}
