//! `set`：修改分区表属性或文件系统属性。
//!
//! 表属性只改分区表；标签与 UUID 走内容档案，需要对应程序可用。

use diskedit_core::fs::Verb;

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
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
