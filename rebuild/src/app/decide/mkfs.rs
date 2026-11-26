//! `mkfs` 的判定：确认分区可写、内容能力允许，然后把建卷交给外部程序。
//!
//! 建卷不写分区表，只改写分区内容；因此计划里只有一步 `MakeFilesystem`，
//! 执行器负责把它翻成按家族挑选的工具调用与残留签名清理。

use diskedit_core::fs::{Support, Verb, catalog};

use crate::app::observe::{Command, FilesystemSnapshot, Request};

use super::filesystem::{admission, finish, identity, region, selected};
use super::step::{Step, StepKind};
use super::{Decision, Plan, Refusal};

/// 判定 `mkfs`。
pub fn decide(snapshot: &FilesystemSnapshot, request: &Request) -> Decision {
    let Command::Mkfs {
        filesystem, label, ..
    } = request.command()
    else {
        return Decision::Refuse(Refusal::TableEdit {
            why: "mkfs reached with a different command".to_owned(),
        });
    };
    let partition = match selected(snapshot) {
        Ok(partition) => partition,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    if let Some(refusal) = admission(snapshot, "mkfs") {
        return Decision::Refuse(refusal);
    }
    let identity = match identity(snapshot) {
        Ok(identity) => identity,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    match catalog::profile(filesystem.content()).support(Verb::Mkfs) {
        Support::Tools(_) => {}
        Support::Unsupported(why) => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: format!("mkfs {}", filesystem.as_str()),
                why: why.to_owned(),
            });
        }
        Support::NotApplicable => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: format!("mkfs {}", filesystem.as_str()),
                why: "this content kind has no volume to create".to_owned(),
            });
        }
    }
    if let Some((program, _)) = snapshot.tools.iter().find(|(_, available)| !available) {
        return Decision::Refuse(Refusal::ToolMissing { program: *program });
    }
    let step = Step::new(
        StepKind::MakeFilesystem {
            partition: partition.number,
            filesystem: *filesystem,
            label: label.clone(),
        },
        vec![region(partition)],
    );
    finish(
        snapshot,
        Plan::write(vec![step], identity),
        format!(
            "created {} on partition #{} (verify with: diskedit info {})",
            filesystem.as_str(),
            partition.number,
            snapshot.target.display
        ),
    )
}
