//! 唯一写通道：把本步的写入限制在本步声明的区间内。
//!
//! 每次写入都要求 [`Recorded`] 令牌，并在通道内检查区间被声明集合覆盖、长度
//! 与区间一致；通道同时记住"写了哪段、写下去的摘要是什么"，后置核对据此回读
//! 比对。原始写原语只在通道内部被调用。

use diskedit_core::region::{Region, RegionSet};

use super::ExecError;
use super::digest::{Digest, Hasher};
use super::token::Recorded;
use crate::platform::TargetHandle;

/// 单次回读的块大小。
const VERIFY_CHUNK: usize = 1 << 20;

/// 受声明区间约束的写通道。
pub struct WriteChannel<'a> {
    target: &'a mut dyn TargetHandle,
    declared: RegionSet,
    written: Vec<(Region, Digest)>,
    writes_done: usize,
    fail_after: Option<usize>,
}

impl<'a> WriteChannel<'a> {
    /// 以本步声明区间建立通道。
    pub fn new(target: &'a mut dyn TargetHandle, declared: &[Region]) -> Self {
        Self {
            target,
            declared: RegionSet::from_regions(declared.iter().copied()),
            written: Vec::new(),
            writes_done: 0,
            fail_after: None,
        }
    }

    /// 在第 `writes` 次写入后注入失败；仅测试使用。
    pub fn with_fail_after(mut self, writes: Option<usize>) -> Self {
        self.fail_after = writes;
        self
    }

    pub fn target_len(&self) -> u64 {
        self.target.len()
    }

    /// 读一段原始字节；读取不受声明区间限制。
    pub fn read_exact(&self, offset: u64, len: usize) -> Result<Vec<u8>, ExecError> {
        let mut buffer = vec![0u8; len];
        let mut filled = 0;
        while filled < len {
            let read = self
                .target
                .read_at(offset + filled as u64, &mut buffer[filled..])
                .map_err(ExecError::from)?;
            if read == 0 {
                return Err(ExecError::Io(format!(
                    "read at {} for {} bytes ended early",
                    offset, len
                )));
            }
            filled += read;
        }
        Ok(buffer)
    }

    /// 写一段字节；区间必须被本步声明覆盖。
    pub fn write(
        &mut self,
        token: &Recorded,
        region: Region,
        bytes: &[u8],
    ) -> Result<(), ExecError> {
        let _ = token;
        if !self.declared.covers(region) {
            return Err(ExecError::InvalidPlan(format!(
                "write region [{}, {}) not declared by this step",
                region.start(),
                region.end()
            )));
        }
        if bytes.len() as u64 != region.len() {
            return Err(ExecError::InvalidPlan(format!(
                "region length {} and byte length {} mismatch",
                region.len(),
                bytes.len()
            )));
        }
        if region.end() > self.target.len() {
            return Err(ExecError::InvalidPlan(format!(
                "write region [{}, {}) ends past the target length {}",
                region.start(),
                region.end(),
                self.target.len()
            )));
        }
        if let Some(limit) = self.fail_after {
            if self.writes_done >= limit {
                return Err(ExecError::Injected(
                    "write channel interrupted during write".into(),
                ));
            }
        }
        let mut offset = region.start();
        let mut written_bytes = 0;
        while written_bytes < bytes.len() {
            let written = self
                .target
                .write_at(offset, &bytes[written_bytes..])
                .map_err(ExecError::from)?;
            if written == 0 {
                return Err(ExecError::Io(format!(
                    "zero progress writing at {}",
                    offset
                )));
            }
            offset += written as u64;
            written_bytes += written;
        }
        self.writes_done += 1;
        let mut hasher = Hasher::new();
        hasher.update(bytes);
        self.written.push((region, hasher.finish()));
        Ok(())
    }

    /// 提交目标缓存。
    pub fn barrier(&mut self) -> Result<(), ExecError> {
        self.target.barrier().map_err(ExecError::from)
    }

    /// 已写入的分段及其摘要。
    pub fn written(&self) -> &[(Region, Digest)] {
        &self.written
    }

    /// 回读并逐个核对已写入分段的摘要。
    pub fn verify_written(&self) -> Result<(), ExecError> {
        for (region, expected) in &self.written {
            let mut hasher = Hasher::new();
            let mut offset = region.start();
            let mut remaining = region.len();
            while remaining > 0 {
                let take = remaining.min(VERIFY_CHUNK as u64) as usize;
                let bytes = self.read_exact(offset, take)?;
                hasher.update(&bytes);
                offset += take as u64;
                remaining -= take as u64;
            }
            if hasher.finish() != *expected {
                return Err(ExecError::Postcondition(format!(
                    "read-back digest mismatch for region [{}, {})",
                    region.start(),
                    region.end()
                )));
            }
        }
        Ok(())
    }
}
