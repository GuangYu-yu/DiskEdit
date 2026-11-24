//! 回滚：按记录中的表区原像，从末项到首项写回目标字节。
//!
//! 只回放携带原像字节的区间；数据区步骤没有原像字节，回滚时不动数据区。
//! 每次写入都经唯一写通道并携带令牌，写完统一交缓存、再逐条回读比对。

use diskedit_core::record::Frame;
use diskedit_core::region::Region;

use crate::app::execute::digest::digest;
use crate::app::execute::transaction;
use crate::app::execute::{ExecError, WriteChannel};
use crate::platform::TargetHandle;

/// 成功回滚的区间数与其中不可回滚的数据区步骤数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub regions: usize,
    pub irreversible: usize,
}

/// 回滚失败；`written` 为已成功写入的区间数，用于区分"未写"与"写到一半"。
#[derive(Debug)]
pub struct Failure {
    pub written: usize,
    pub error: ExecError,
}

/// 回滚记录中的表区原像。
pub fn apply(
    target: &mut dyn TargetHandle,
    frames: &[Frame],
    scope: Scope,
) -> Result<Summary, Failure> {
    let regions = collect(frames, scope)?;
    if regions.is_empty() {
        // 只有数据区步骤时，表区回放没有可写的字节；记录照样清理，报告零区间。
        return Ok(Summary {
            regions: 0,
            irreversible: frames.iter().filter(|frame| frame.irreversible).count(),
        });
    }
    let declared: Vec<Region> = regions.iter().map(|(region, _, _)| *region).collect();
    let token = transaction::recovery_token(frames.len() as u64);
    let mut channel = WriteChannel::new(target, &declared);
    for (index, (region, data, expected)) in regions.iter().enumerate() {
        if digest(data) != *expected {
            return Err(Failure {
                written: index,
                error: ExecError::Record(
                    "preimage content and recorded digest mismatch".to_owned(),
                ),
            });
        }
        if let Err(error) = channel.write(&token, *region, data) {
            return Err(Failure {
                written: index,
                error,
            });
        }
    }
    let written = regions.len();
    if let Err(error) = channel.barrier() {
        return Err(Failure { written, error });
    }
    channel
        .verify_written()
        .map_err(|error| Failure { written, error })?;
    Ok(Summary {
        regions: written,
        irreversible: frames.iter().filter(|frame| frame.irreversible).count(),
    })
}

/// 一条待回滚的表区原像：区间、原始字节与登记摘要。
type Rolling<'a> = (Region, &'a [u8], [u8; 16]);

/// 回滚范围。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// 回放记录里所有携带原像的区间（默认恢复）。
    All,
    /// 只回放分区表区原像，跳过数据区步骤可能登记的其它区间。
    TableOnly,
}

/// 分区表区的步骤意图；只有这些帧写下的原像属于表区。
const TABLE_INTENTS: &[&str] = &[
    "write-table",
    "add-partition",
    "delete-partition",
    "resize-partition",
    "set-property",
];

/// 数据区步骤意图；数据一旦写过，默认恢复不能假装能把它拉回。
pub const DATA_INTENTS: &[&str] = &[
    "make-filesystem",
    "grow-filesystem",
    "shrink-filesystem",
    "copy-partition",
    "move-partition",
];

/// `intent` 是否属于给定的意图集合。
pub fn intent_is(intent: &[u8], set: &[&str]) -> bool {
    std::str::from_utf8(intent).is_ok_and(|text| set.contains(&text))
}

/// 从末项到首项收集携带原像字节的区间。
fn collect<'a>(frames: &'a [Frame], scope: Scope) -> Result<Vec<Rolling<'a>>, Failure> {
    let mut regions = Vec::new();
    for frame in frames.iter().rev() {
        if scope == Scope::TableOnly && !intent_is(&frame.intent, TABLE_INTENTS) {
            continue;
        }
        for preimage in frame.regions.iter().rev() {
            if preimage.data.is_empty() {
                continue;
            }
            if preimage.data.len() as u64 != preimage.len {
                return Err(Failure {
                    written: 0,
                    error: ExecError::Record(
                        "preimage length and region length mismatch".to_owned(),
                    ),
                });
            }
            let region = Region::sized(preimage.offset, preimage.len).map_err(|_| Failure {
                written: 0,
                error: ExecError::Record("preimage region out of range or inverted".to_owned()),
            })?;
            regions.push((region, preimage.data.as_slice(), preimage.digest));
        }
    }
    if regions.is_empty() && scope == Scope::All {
        return Err(Failure {
            written: 0,
            error: ExecError::Record("record has no table preimages to roll back".to_owned()),
        });
    }
    Ok(regions)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use diskedit_core::record::frame::{Frame, FrameKind, Preimage};

    use super::*;
    use crate::platform::{PlatformError, PlatformResult, TargetKind};

    struct Bytes(Vec<u8>);

    impl TargetHandle for Bytes {
        fn path(&self) -> &Path {
            Path::new("<memory>")
        }
        fn kind(&self) -> TargetKind {
            TargetKind::File
        }
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        fn sector_size(&self) -> Option<u64> {
            Some(512)
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start >= self.0.len() {
                return Ok(0);
            }
            let len = buf.len().min(self.0.len() - start);
            buf[..len].copy_from_slice(&self.0[start..start + len]);
            Ok(len)
        }
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            let end = start + buf.len();
            if end > self.0.len() {
                return Err(PlatformError::parse("越界写"));
            }
            self.0[start..end].copy_from_slice(buf);
            Ok(buf.len())
        }
        fn barrier(&mut self) -> PlatformResult<()> {
            Ok(())
        }
        fn lock_exclusive(&mut self) -> PlatformResult<()> {
            Ok(())
        }
    }

    fn frame(seq: u32, intent: &str, offset: u64, data: &[u8]) -> Frame {
        Frame {
            seq,
            kind: FrameKind::Preimage,
            completed: false,
            irreversible: false,
            intent: intent.as_bytes().to_vec(),
            regions: vec![Preimage {
                offset,
                len: data.len() as u64,
                digest: digest(data),
                data: data.to_vec(),
            }],
        }
    }

    #[test]
    fn table_scope_skips_data_region_preimages() {
        // 合成一份同时含表区与数据区原像的记录：默认回滚两者都写，表区回放只写表区。
        let table = frame(0, "write-table", 0, &[0xAA; 4]);
        let data = frame(1, "grow-filesystem", 16, &[0xBB; 4]);
        let frames = [table, data];

        let mut all = Bytes(vec![0; 32]);
        let summary = apply(&mut all, &frames, Scope::All).expect("默认回滚");
        assert_eq!(summary.regions, 2);
        assert_eq!(&all.0[0..4], &[0xAA; 4]);
        assert_eq!(&all.0[16..20], &[0xBB; 4]);

        let mut table_only = Bytes(vec![0; 32]);
        let summary = apply(&mut table_only, &frames, Scope::TableOnly).expect("表区回放");
        assert_eq!(summary.regions, 1);
        assert_eq!(&table_only.0[0..4], &[0xAA; 4]);
        assert_eq!(&table_only.0[16..20], &[0; 4], "数据区不得被表区回放改动");
    }

    #[test]
    fn table_scope_without_table_frames_is_a_no_op() {
        let frames = [frame(0, "grow-filesystem", 0, &[0x11; 4])];
        let mut bytes = Bytes(vec![0; 8]);
        let summary = apply(&mut bytes, &frames, Scope::TableOnly).expect("无表区原像");
        assert_eq!(summary.regions, 0);
        assert_eq!(bytes.0, vec![0; 8]);
    }
}
