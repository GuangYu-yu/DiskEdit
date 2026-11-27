//! 后置条件：写入结束后回读已写分段并核对摘要。
//!
//! 核对对象是写通道实际写下的分段，不是整块目标；未写入的字节不参与判定。

use super::{ExecError, WriteChannel};

/// 已写分段的统计。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub segments: usize,
    pub bytes: u64,
}

/// 统计已写分段。
pub fn summary(channel: &WriteChannel<'_>) -> Summary {
    let mut bytes = 0u64;
    for (region, _) in channel.written() {
        bytes = bytes.saturating_add(region.len());
    }
    Summary {
        segments: channel.written().len(),
        bytes,
    }
}

/// 回读核对；失败即表示目标字节与写入意图不一致。
pub fn verify(channel: &WriteChannel<'_>) -> Result<(), ExecError> {
    channel.verify_written()
}
