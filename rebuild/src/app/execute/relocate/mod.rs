//! 分区搬移：按方向选择块序，避免源与目标自重叠时覆盖未读数据。
//!
//! 搬移只读写数据区；分区表的新位置由后续 `ResizePartition` 步骤提交。搬移
//! 已写的块不回收、不清零旧位置，因此记录把这一步标为不可逆。

use diskedit_core::region::Region;

use super::digest::{Digest, Hasher};
use super::token::Recorded;
use super::{ExecError, WriteChannel};

/// 单块字节数。
pub const CHUNK_BYTES: u64 = 1 << 20;

/// 搬移方向：目标起点在源之后时从尾部向前搬，否则从头部向后搬。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

impl Direction {
    pub fn between(source: Region, destination: Region) -> Self {
        if destination.start() > source.start() {
            Self::Backward
        } else {
            Self::Forward
        }
    }
}

/// 一次搬移的绑定：源、目标、块大小与总块数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub source: Region,
    pub destination: Region,
    pub chunk: u64,
    pub chunks: u64,
}

impl Binding {
    /// 两个区间长度必须一致且非空。
    pub fn new(source: Region, destination: Region, chunk: u64) -> Result<Self, ExecError> {
        if source.len() != destination.len() {
            return Err(ExecError::InvalidPlan(
                "搬移的源与目标长度不一致".to_owned(),
            ));
        }
        if source.is_empty() {
            return Err(ExecError::InvalidPlan("搬移区间为空".to_owned()));
        }
        let chunk = chunk.max(1).min(source.len());
        let chunks = source.len().div_ceil(chunk);
        Ok(Self {
            source,
            destination,
            chunk,
            chunks,
        })
    }

    /// 方向。
    pub fn direction(self) -> Direction {
        Direction::between(self.source, self.destination)
    }

    /// 按下标取第 `index` 块在源与目标上的区间。
    pub fn piece(self, index: u64) -> Result<(Region, Region), ExecError> {
        if index >= self.chunks {
            return Err(ExecError::InvalidPlan("块下标越界".to_owned()));
        }
        let offset = index * self.chunk;
        let len = self.chunk.min(self.source.len() - offset);
        let source = Region::sized(self.source.start() + offset, len)
            .map_err(|error| ExecError::InvalidPlan(format!("空区间: {error:?}")))?;
        let destination = Region::sized(self.destination.start() + offset, len)
            .map_err(|error| ExecError::InvalidPlan(format!("空区间: {error:?}")))?;
        Ok((source, destination))
    }

    /// 按方向的块下标序列。
    pub fn order(self) -> ChunkOrder {
        ChunkOrder {
            front: 0,
            back: self.chunks,
            direction: self.direction(),
        }
    }
}

/// 按方向产出块下标。
#[derive(Clone, Debug)]
pub struct ChunkOrder {
    front: u64,
    back: u64,
    direction: Direction,
}

impl Iterator for ChunkOrder {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        if self.front >= self.back {
            return None;
        }
        Some(match self.direction {
            Direction::Forward => {
                let index = self.front;
                self.front += 1;
                index
            }
            Direction::Backward => {
                self.back -= 1;
                self.back
            }
        })
    }
}

/// 搬移进度：已完成的块数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Checkpoint {
    completed: u64,
}

impl Checkpoint {
    pub const fn completed(self) -> u64 {
        self.completed
    }

    pub fn record(&mut self) {
        self.completed += 1;
    }
}

/// 搬移中的故障注入点。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelocateFaults {
    /// 在第 `index` 块开始前中断。
    pub stop_before_chunk: Option<u64>,
}

/// 搬移结果：完成块数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moved {
    pub chunks: u64,
}

/// 执行搬移。
pub fn apply(
    binding: Binding,
    channel: &mut WriteChannel<'_>,
    token: &Recorded,
    faults: RelocateFaults,
) -> Result<Moved, ExecError> {
    let mut checkpoint = Checkpoint::default();
    for index in binding.order() {
        if faults.stop_before_chunk == Some(index) {
            return Err(ExecError::Injected(format!("搬移在第 {index} 块前中断")));
        }
        let (source, destination) = binding.piece(index)?;
        let bytes = channel.read_exact(source.start(), source.len() as usize)?;
        channel.write(token, destination, &bytes)?;
        checkpoint.record();
    }
    Ok(Moved {
        chunks: checkpoint.completed(),
    })
}

/// 搬移后核对：目标块的摘要与源块一致。
pub fn verify(binding: Binding, channel: &mut WriteChannel<'_>) -> Result<(), ExecError> {
    for index in binding.order() {
        let (source, destination) = binding.piece(index)?;
        let source_digest = digest_region(channel, source)?;
        let destination_digest = digest_region(channel, destination)?;
        if source_digest != destination_digest {
            return Err(ExecError::Postcondition(format!(
                "第 {index} 块搬移后与源不一致"
            )));
        }
    }
    Ok(())
}

fn digest_region(channel: &WriteChannel<'_>, region: Region) -> Result<Digest, ExecError> {
    let mut hasher = Hasher::new();
    let mut offset = region.start();
    let mut remaining = region.len();
    while remaining > 0 {
        let take = remaining.min(CHUNK_BYTES) as usize;
        let bytes = channel.read_exact(offset, take)?;
        hasher.update(&bytes);
        offset += take as u64;
        remaining -= take as u64;
    }
    Ok(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(start: u64, len: u64) -> Region {
        Region::sized(start, len).expect("区间应成立")
    }

    #[test]
    fn direction_follows_relative_position() {
        let source = region(0, 4096);
        assert_eq!(
            Direction::between(source, region(4096, 4096)),
            Direction::Backward
        );
        assert_eq!(
            Direction::between(region(4096, 4096), source),
            Direction::Forward
        );
    }

    #[test]
    fn backward_order_reads_tail_first() {
        let binding = Binding::new(region(0, 300), region(100, 300), 100).expect("绑定应成立");
        assert_eq!(binding.chunks, 3);
        assert_eq!(binding.order().collect::<Vec<_>>(), vec![2, 1, 0]);
    }

    #[test]
    fn forward_order_reads_head_first() {
        let binding = Binding::new(region(300, 300), region(100, 300), 100).expect("绑定应成立");
        assert_eq!(binding.order().collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn piece_lengths_trim_last_chunk() {
        let binding = Binding::new(region(0, 250), region(500, 250), 100).expect("绑定应成立");
        let (source, destination) = binding.piece(2).expect("末块应成立");
        assert_eq!(source.len(), 50);
        assert_eq!(destination.start(), 700);
    }

    #[test]
    fn mismatched_lengths_are_rejected() {
        assert!(Binding::new(region(0, 100), region(0, 200), 64).is_err());
        assert!(Binding::new(region(0, 0), region(0, 0), 64).is_err());
    }
}
