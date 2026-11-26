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
        if let Some(limit) = self.fail_after
            && self.writes_done >= limit
        {
            return Err(ExecError::Injected(
                "write channel interrupted during write".into(),
            ));
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::app::execute::transaction::Mint;
    use crate::platform::{PlatformResult, TargetKind};

    /// 内存目标：只用于验证写通道的边界与回读，不触盘。
    struct MemoryTarget {
        bytes: Vec<u8>,
    }

    impl TargetHandle for MemoryTarget {
        fn path(&self) -> &Path {
            Path::new("memory")
        }

        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }

        fn kind(&self) -> TargetKind {
            TargetKind::File
        }

        fn sector_size(&self) -> Option<u64> {
            Some(512)
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
            let start = offset as usize;
            let end = (start + buf.len()).min(self.bytes.len());
            let n = end - start;
            buf[..n].copy_from_slice(&self.bytes[start..end]);
            Ok(n)
        }

        fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
            let start = offset as usize;
            let end = (start + buf.len()).min(self.bytes.len());
            let n = end - start;
            self.bytes[start..end].copy_from_slice(&buf[..n]);
            Ok(n)
        }

        fn barrier(&mut self) -> PlatformResult<()> {
            Ok(())
        }

        fn refresh_partitions(&mut self) -> PlatformResult<()> {
            Ok(())
        }
    }

    fn token() -> Recorded {
        Recorded::mint(&Mint::new(), 1)
    }

    // covers: I3
    #[test]
    fn a_write_outside_the_declared_regions_is_rejected() {
        let mut target = MemoryTarget {
            bytes: vec![0u8; 4096],
        };
        let declared = [Region::sized(0, 512).expect("区间")];
        let mut channel = WriteChannel::new(&mut target, &declared);
        let region = Region::sized(512, 512).expect("区间");
        let error = channel
            .write(&token(), region, &[0u8; 512])
            .expect_err("未声明的区间必须被拒绝");
        assert!(
            error.to_string().contains("not declared"),
            "实际错误：{error}"
        );
    }

    // covers: B20, I11
    #[test]
    fn a_write_past_the_target_length_is_rejected() {
        let mut target = MemoryTarget {
            bytes: vec![0u8; 4096],
        };
        let declared = [Region::sized(0, 8192).expect("区间")];
        let mut channel = WriteChannel::new(&mut target, &declared);
        let region = Region::sized(4096, 512).expect("区间");
        let error = channel
            .write(&token(), region, &[0u8; 512])
            .expect_err("越过目标长度的写入必须被拒绝");
        assert!(
            error.to_string().contains("past the target length"),
            "实际错误：{error}"
        );
    }

    // covers: I3, I11
    #[test]
    fn a_declared_in_bounds_write_reads_back_identically() {
        let mut target = MemoryTarget {
            bytes: vec![0u8; 4096],
        };
        let declared = [Region::sized(0, 1024).expect("区间")];
        let mut channel = WriteChannel::new(&mut target, &declared);
        let region = Region::sized(0, 512).expect("区间");
        channel
            .write(&token(), region, &[0x5au8; 512])
            .expect("区间内的写入应成功");
        channel.verify_written().expect("回读摘要应一致");
    }
}
