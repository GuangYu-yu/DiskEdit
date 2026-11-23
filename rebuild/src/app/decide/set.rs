//! `set`：修改分区表属性或文件系统属性。
//!
//! 表属性只改分区表；标签与 UUID 走内容档案，需要对应程序可用。

use diskedit_core::fs::Verb;

use super::common::{admission, capacity};
use super::plan::Plan;
use super::refusal::Refusal;
use super::step::{Step, StepKind};
use crate::app::observe::TableFormat;
use crate::app::observe::{SetProperty, Snapshot};

pub fn plan(
    snapshot: &Snapshot,
    partition: u32,
    property: SetProperty,
    value: String,
) -> Result<Plan, Refusal> {
    let admitted = admission::admit(snapshot)?;
    let facts = admission::partition(snapshot, partition)?;
    match property {
        SetProperty::Name | SetProperty::Flag | SetProperty::Type => {
            let table = admission::table(snapshot)?;
            let format = table.format();
            if format == TableFormat::Mbr && property == SetProperty::Name {
                return Err(Refusal::ContentUnsupported {
                    what: "MBR 分区名".to_owned(),
                    why: "MBR 分区没有名字".to_owned(),
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
                    what: format!("{} 属性", content.as_str()),
                    why: "该内容没有可修改的属性".to_owned(),
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
                },
                vec![region],
            )];
            Plan::write(steps, admitted.identity)
        }
    }
}
