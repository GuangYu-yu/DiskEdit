//! `resizefs` 的判定：把分区内的文件系统扩到分区容量。
//!
//! 离线形态只扩容：文件系统末尾之前的分区空间此刻已经属于分区，工具把文件系统
//! 长满即可。带尺寸或挂载点形态属于在线路径，另行接入。

use diskedit_core::fs::{ContentKind, Support, Verb, catalog};

use crate::app::observe::{Command, FilesystemSnapshot, Request};

use super::filesystem::{admission, finish, identity, region, selected};
use super::step::{Step, StepKind};
use super::{Decision, Plan, Refusal};

/// 判定离线的 `resizefs <TARGET>:N`。
pub fn decide(snapshot: &FilesystemSnapshot, request: &Request) -> Decision {
    match request.command() {
        Command::Resizefs { partition, .. } if partition.is_some() => {}
        Command::Resizefs { .. } => return mountpoint(),
        Command::Info { .. }
        | Command::Inspect
        | Command::RecordsList
        | Command::RecordsVerify { .. }
        | Command::RecordsScan
        | Command::Resize(_)
        | Command::Move(_)
        | Command::Copy(_)
        | Command::Delete { .. }
        | Command::Set(_)
        | Command::Check { .. }
        | Command::Mkfs { .. }
        | Command::Recover(_)
        | Command::New { .. }
        | Command::Add(_) => {
            return Decision::Refuse(Refusal::TableEdit {
                why: "resizefs reached with a different command".to_owned(),
            });
        }
    }
    if let Some(refusal) = admission(snapshot, "resizefs") {
        return Decision::Refuse(refusal);
    }
    let partition = match selected(snapshot) {
        Ok(partition) => partition,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let identity = match identity(snapshot) {
        Ok(identity) => identity,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let content = partition.content;
    match content {
        ContentKind::Unknown => {
            return Decision::Refuse(Refusal::FilesystemUnknown {
                number: partition.number,
            });
        }
        ContentKind::Swap => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: "resizefs swap".to_owned(),
                why: "a swap area has no in-place resize; rebuild it at the target size".to_owned(),
            });
        }
        ContentKind::Lvm2Pv => {
            return Decision::Refuse(Refusal::NoFilesystem {
                number: partition.number,
                what: "an LVM physical volume".to_owned(),
            });
        }
        ContentKind::Filesystem(_) => {}
        ContentKind::Luks | ContentKind::BitLocker => {
            return Decision::Refuse(Refusal::NoFilesystem {
                number: partition.number,
                what: content.as_str().to_owned(),
            });
        }
    }
    match catalog::profile(content).support(Verb::Grow) {
        Support::Tools(_) => {}
        Support::Unsupported(why) => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: format!("resizefs {}", content.as_str()),
                why: why.to_owned(),
            });
        }
        Support::NotApplicable => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: format!("resizefs {}", content.as_str()),
                why: "this content kind carries no filesystem to resize".to_owned(),
            });
        }
    }
    let Some(filesystem) = content.filesystem() else {
        return Decision::Refuse(Refusal::ContentUnsupported {
            what: format!("resizefs {}", content.as_str()),
            why: "this content kind carries no filesystem to resize".to_owned(),
        });
    };
    if let Some((program, _)) = snapshot.tools.iter().find(|(_, available)| !available) {
        return Decision::Refuse(Refusal::ToolMissing { program: *program });
    }
    let step = Step::new(
        StepKind::GrowFilesystem {
            partition: partition.number,
            filesystem,
            bytes: partition.bytes,
        },
        vec![region(partition)],
    );
    finish(
        snapshot,
        Plan::write(vec![step], identity),
        format!(
            "resized the filesystem on partition #{} (verify with: diskedit info {})",
            partition.number, snapshot.target.display
        ),
    )
}

/// 挂载点形态：在线扩容在挂载视图接入后处理，这里给出可执行的替代形态。
pub fn mountpoint() -> Decision {
    Decision::Refuse(Refusal::ContentUnsupported {
        what: "resizefs at a mount point".to_owned(),
        why: "pass <TARGET>:N to resize the filesystem inside a partition".to_owned(),
    })
}
