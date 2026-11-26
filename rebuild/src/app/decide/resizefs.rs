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
        Command::Resizefs { bytes, .. } => return online(snapshot, *bytes),
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

/// 挂载点形态：在用户已有的挂载上把文件系统撑满所在分区。
///
/// 不写分区表、不卸载挂载。尺寸只允许省略或等于分区容量：大于容量要先改表，
/// 小于容量是收缩，两者都在任何写盘之前拒绝。
fn online(snapshot: &FilesystemSnapshot, requested: Option<u64>) -> Decision {
    use crate::platform::MountFacts;

    let Some(MountFacts::Mounted {
        mount_point,
        device,
        disk,
        partition: number,
    }) = snapshot.mount.clone()
    else {
        return Decision::Refuse(Refusal::FactUnknown {
            what: "the mount point did not resolve to a mounted partition".to_owned(),
        });
    };
    let Some(selected) = snapshot
        .selected
        .as_ref()
        .filter(|facts| facts.number == number)
    else {
        return Decision::Refuse(Refusal::PartitionMissing { number });
    };
    if let Some(requested) = requested {
        if requested > selected.bytes {
            return Decision::Refuse(Refusal::CapacityInsufficient {
                needed: requested,
                available: selected.bytes,
            });
        }
        if requested < selected.bytes {
            return Decision::Refuse(Refusal::OnlineShrink {
                why: "this form grows the filesystem to fill its partition and never changes the \
                      partition table"
                    .to_owned(),
            });
        }
    }
    let content = selected.content;
    let Some(filesystem) = content.filesystem() else {
        return Decision::Refuse(Refusal::ContentUnsupported {
            what: format!("resizefs {}", content.as_str()),
            why: "this content kind carries no filesystem to resize".to_owned(),
        });
    };
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
    if let Some((program, _)) = snapshot.tools.iter().find(|(_, available)| !available) {
        return Decision::Refuse(Refusal::ToolMissing { program: *program });
    }
    let Ok(identity) = identity(snapshot) else {
        return Decision::Refuse(Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        });
    };
    let step = Step::new(
        StepKind::GrowFilesystemOnline {
            mount_point,
            device,
            disk,
            partition: number,
            filesystem,
            bytes: selected.bytes,
        },
        Vec::new(),
    );
    finish(
        snapshot,
        Plan::external(vec![step], identity),
        format!("resized the filesystem on partition #{number} through its mounted volume"),
    )
}
