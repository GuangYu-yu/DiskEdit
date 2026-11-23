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
pub fn apply(target: &mut dyn TargetHandle, frames: &[Frame]) -> Result<Summary, Failure> {
    let regions = collect(frames)?;
    let declared: Vec<Region> = regions.iter().map(|(region, _, _)| *region).collect();
    let token = transaction::recovery_token(frames.len() as u64);
    let mut channel = WriteChannel::new(target, &declared);
    for (index, (region, data, expected)) in regions.iter().enumerate() {
        if digest(data) != *expected {
            return Err(Failure {
                written: index,
                error: ExecError::Record("原像内容与登记的摘要不一致".to_owned()),
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

/// 从末项到首项收集携带原像字节的区间。
fn collect(frames: &[Frame]) -> Result<Vec<Rolling<'_>>, Failure> {
    let mut regions = Vec::new();
    for frame in frames.iter().rev() {
        for preimage in frame.regions.iter().rev() {
            if preimage.data.is_empty() {
                continue;
            }
            if preimage.data.len() as u64 != preimage.len {
                return Err(Failure {
                    written: 0,
                    error: ExecError::Record("原像长度与区间长度不一致".to_owned()),
                });
            }
            let region = Region::sized(preimage.offset, preimage.len).map_err(|_| Failure {
                written: 0,
                error: ExecError::Record("原像区间越界或倒置".to_owned()),
            })?;
            regions.push((region, preimage.data.as_slice(), preimage.digest));
        }
    }
    if regions.is_empty() {
        return Err(Failure {
            written: 0,
            error: ExecError::Record("记录中没有可回滚的表区原像".to_owned()),
        });
    }
    Ok(regions)
}
