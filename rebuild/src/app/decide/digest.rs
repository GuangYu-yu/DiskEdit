//! 计划摘要：把目标指纹与步骤序列折叠成 32 字节。
//!
//! 摘要是确认与记录使用的比对值，不是密码学签名；四条独立的 FNV-1a 通道
//! 足以发现计划内容被改动。

use diskedit_core::fingerprint::{Fingerprint, Strength};

use super::step::{Access, Step, StepKind};
use crate::app::observe::{RecoverMode, SetProperty, TableFormat};

/// 计算摘要。
pub fn plan_digest(identity: Option<Fingerprint>, steps: &[Step]) -> [u8; 32] {
    let mut lanes = [Lane::new(0xcbf2_9ce4_8422_2325); 4];
    match identity {
        Some(identity) => {
            push_bytes(&mut lanes, b"identity");
            push_bytes(&mut lanes, &identity.bytes());
            push_bytes(&mut lanes, &[strength_tag(identity)]);
        }
        None => push_bytes(&mut lanes, b"no-identity"),
    }
    for step in steps {
        push_bytes(&mut lanes, &[access_tag(step.access())]);
        let mut encoded = Vec::new();
        encode_kind(&mut encoded, &step.kind);
        push_bytes(&mut lanes, &encoded);
        push_bytes(&mut lanes, &(step.regions.len() as u64).to_le_bytes());
        for region in &step.regions {
            push_bytes(&mut lanes, &region.start().to_le_bytes());
            push_bytes(&mut lanes, &region.end().to_le_bytes());
        }
    }
    let mut out = [0u8; 32];
    for (slot, lane) in out.chunks_exact_mut(8).zip(lanes) {
        slot.copy_from_slice(&lane.finish().to_le_bytes());
    }
    out
}

/// 请求摘要：只绑定"新建哪张表、是否覆盖、块大小与目标身份"，不含随机 GUID。
///
/// 记录的续跑判定用它：同一请求重跑得到同一摘要，因此可以从中断处继续；
/// 而完整计划摘要仍绑定实际写出的每个字节。
pub fn request_digest(
    identity: Option<Fingerprint>,
    format: TableFormat,
    overwrite: bool,
    sector_size: u64,
) -> [u8; 32] {
    let mut lanes = [Lane::new(0xcbf2_9ce4_8422_2325); 4];
    match identity {
        Some(identity) => {
            push_bytes(&mut lanes, b"identity");
            push_bytes(&mut lanes, &identity.bytes());
            push_bytes(&mut lanes, &[strength_tag(identity)]);
        }
        None => push_bytes(&mut lanes, b"no-identity"),
    }
    push_bytes(&mut lanes, b"new-table");
    push_bytes(&mut lanes, &[format_tag(format), u8::from(overwrite)]);
    push_bytes(&mut lanes, &sector_size.to_le_bytes());
    let mut out = [0u8; 32];
    for (slot, lane) in out.chunks_exact_mut(8).zip(lanes) {
        slot.copy_from_slice(&lane.finish().to_le_bytes());
    }
    out
}

#[derive(Clone, Copy)]
struct Lane(u64);

impl Lane {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn feed(&mut self, byte: u8) {
        self.0 ^= u64::from(byte);
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }

    const fn finish(self) -> u64 {
        self.0
    }
}

fn push_bytes(lanes: &mut [Lane; 4], bytes: &[u8]) {
    for (index, lane) in lanes.iter_mut().enumerate() {
        // 每条通道加入通道号，避免不同通道对同一输入产生相同序列。
        lane.feed(index as u8);
        lane.feed(0x7c);
        for byte in bytes {
            lane.feed(*byte);
        }
    }
}

fn encode_kind(out: &mut Vec<u8>, kind: &StepKind) {
    match kind {
        StepKind::Inspect => out.push(1),
        StepKind::Check { partition } => {
            out.push(2);
            out.extend_from_slice(&partition.to_le_bytes());
        }
        StepKind::ListRecords => out.push(3),
        StepKind::VerifyRecord { token } => {
            out.push(4);
            out.extend_from_slice(token.as_bytes());
        }
        StepKind::ScanRecords => out.push(5),
        StepKind::WriteTable {
            format,
            segment_size,
            stamp,
            segments,
        } => {
            out.push(6);
            out.push(format_tag(*format));
            out.extend_from_slice(&segment_size.to_le_bytes());
            out.extend_from_slice(stamp);
            out.extend_from_slice(&(segments.len() as u64).to_le_bytes());
            for segment in segments {
                out.extend_from_slice(&segment.region.start().to_le_bytes());
                out.extend_from_slice(&segment.region.end().to_le_bytes());
                out.extend_from_slice(&(segment.bytes.len() as u64).to_le_bytes());
                out.extend_from_slice(&segment.bytes);
            }
        }
        StepKind::AddPartition {
            format,
            start_lba,
            sectors,
            type_guid,
            name,
            part_number,
            unique,
        } => {
            out.push(7);
            out.push(format_tag(*format));
            out.extend_from_slice(&start_lba.to_le_bytes());
            out.extend_from_slice(&sectors.to_le_bytes());
            out.extend_from_slice(&type_guid.0);
            out.extend_from_slice(name.as_bytes());
            match part_number {
                Some(number) => {
                    out.push(1);
                    out.extend_from_slice(&number.to_le_bytes());
                }
                None => out.push(0),
            }
            match unique {
                Some(guid) => {
                    out.push(1);
                    out.extend_from_slice(&guid.0);
                }
                None => out.push(0),
            }
        }
        StepKind::DeletePartition { format, partition } => {
            out.push(8);
            out.push(format_tag(*format));
            out.extend_from_slice(&partition.to_le_bytes());
        }
        StepKind::ResizePartition {
            format,
            partition,
            start_lba,
            sectors,
        } => {
            out.push(9);
            out.push(format_tag(*format));
            out.extend_from_slice(&partition.to_le_bytes());
            out.extend_from_slice(&start_lba.to_le_bytes());
            out.extend_from_slice(&sectors.to_le_bytes());
        }
        StepKind::SetProperty {
            partition,
            property,
            value,
            content,
        } => {
            out.push(10);
            out.extend_from_slice(&partition.to_le_bytes());
            out.push(property_tag(*property));
            match content {
                Some(content) => {
                    out.push(1);
                    out.extend_from_slice(content.as_str().as_bytes());
                }
                None => out.push(0),
            }
            out.extend_from_slice(value.as_bytes());
        }
        StepKind::MakeFilesystem {
            partition,
            filesystem,
            label,
        } => {
            out.push(11);
            out.extend_from_slice(&partition.to_le_bytes());
            out.extend_from_slice(filesystem.as_str().as_bytes());
            match label {
                Some(text) => {
                    out.push(1);
                    out.extend_from_slice(text.as_bytes());
                }
                None => out.push(0),
            }
        }
        StepKind::GrowFilesystem {
            partition,
            filesystem,
            bytes,
        } => {
            out.push(12);
            out.extend_from_slice(&partition.to_le_bytes());
            out.extend_from_slice(filesystem.as_str().as_bytes());
            out.extend_from_slice(&bytes.to_le_bytes());
        }
        StepKind::ShrinkFilesystem {
            partition,
            filesystem,
            bytes,
        } => {
            out.push(13);
            out.extend_from_slice(&partition.to_le_bytes());
            out.extend_from_slice(filesystem.as_str().as_bytes());
            out.extend_from_slice(&bytes.to_le_bytes());
        }
        StepKind::CopyPartition {
            source,
            source_lba,
            start_lba,
            sectors,
            chunk,
        } => {
            out.push(14);
            out.extend_from_slice(&source.to_le_bytes());
            out.extend_from_slice(&source_lba.to_le_bytes());
            out.extend_from_slice(&start_lba.to_le_bytes());
            out.extend_from_slice(&sectors.to_le_bytes());
            out.extend_from_slice(&chunk.to_le_bytes());
        }
        StepKind::MovePartition {
            partition,
            source_lba,
            offset_sectors,
            chunk,
            ntfs_hidden,
        } => {
            out.push(15);
            out.extend_from_slice(&partition.to_le_bytes());
            out.extend_from_slice(&source_lba.to_le_bytes());
            out.extend_from_slice(&offset_sectors.to_le_bytes());
            out.extend_from_slice(&chunk.to_le_bytes());
            out.push(u8::from(*ntfs_hidden));
        }
        StepKind::RefreshKernelView {
            disk,
            partition,
            start_bytes,
            bytes,
        } => {
            out.push(16);
            out.extend_from_slice(&partition.to_le_bytes());
            out.extend_from_slice(&start_bytes.to_le_bytes());
            out.extend_from_slice(&bytes.to_le_bytes());
            out.extend_from_slice(disk.to_string_lossy().as_bytes());
            out.push(0);
        }
        StepKind::RecoverRecords { mode } => {
            out.push(17);
            out.push(recover_tag(*mode));
        }
        StepKind::GrowFilesystemOnline {
            mount_point,
            device,
            disk,
            partition,
            filesystem,
            bytes,
        } => {
            out.push(18);
            out.extend_from_slice(&partition.to_le_bytes());
            out.extend_from_slice(filesystem.as_str().as_bytes());
            out.push(0);
            out.extend_from_slice(&bytes.to_le_bytes());
            for path in [disk, device, mount_point] {
                out.extend_from_slice(path.to_string_lossy().as_bytes());
                out.push(0);
            }
        }
        StepKind::LvmPvShrink {
            partition,
            start_bytes,
            bytes,
        } => {
            out.push(19);
            out.extend_from_slice(&partition.to_le_bytes());
            out.extend_from_slice(&start_bytes.to_le_bytes());
            out.extend_from_slice(&bytes.to_le_bytes());
        }
        StepKind::LvmGrowChain {
            partition,
            start_bytes,
            old_bytes,
            new_bytes,
            grow_lv,
            lv,
        } => {
            out.push(20);
            out.extend_from_slice(&partition.to_le_bytes());
            out.extend_from_slice(&start_bytes.to_le_bytes());
            out.extend_from_slice(&old_bytes.to_le_bytes());
            out.extend_from_slice(&new_bytes.to_le_bytes());
            out.push(u8::from(*grow_lv));
            match lv {
                Some(text) => {
                    out.push(1);
                    out.extend_from_slice(text.as_bytes());
                }
                None => out.push(0),
            }
        }
    }
}

const fn recover_tag(mode: RecoverMode) -> u8 {
    match mode {
        RecoverMode::Default => 1,
        RecoverMode::ReplayTableOnly => 2,
        RecoverMode::Release {
            discard_replay: false,
        } => 3,
        RecoverMode::Release {
            discard_replay: true,
        } => 4,
    }
}

fn property_tag(property: SetProperty) -> u8 {
    match property {
        SetProperty::Name => 1,
        SetProperty::Label => 2,
        SetProperty::Uuid => 3,
        SetProperty::Flag => 4,
        SetProperty::Type => 5,
    }
}

const fn access_tag(access: Access) -> u8 {
    match access {
        Access::ReadOnly => 1,
        Access::WritesTarget => 2,
        Access::WritesExternalState => 3,
    }
}

const fn format_tag(format: TableFormat) -> u8 {
    match format {
        TableFormat::Gpt => 1,
        TableFormat::Mbr => 2,
    }
}

const fn strength_tag(identity: Fingerprint) -> u8 {
    match identity.strength() {
        Strength::Strong => 1,
        Strength::Unusable => 2,
    }
}
