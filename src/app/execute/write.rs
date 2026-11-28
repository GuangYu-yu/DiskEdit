//! 唯一写通道：把本步的写入限制在本步声明的区间内。
//!
//! 每次写入都要求 [`Recorded`] 令牌，并在通道内检查区间被声明集合覆盖、长度
//! 与区间一致；通道同时记住"写了哪段、写下去的摘要是什么"，后置核对据此回读
//! 比对。原始写原语只在通道内部被调用。

use diskedit_core::region::{Region, RegionSet};

use super::ExecError;
use super::digest::{Digest, Hasher};
use super::token::{Recorded, RegionDigest, SessionId};
use crate::platform::TargetHandle;

/// 单次回读的块大小。
const VERIFY_CHUNK: usize = 1 << 20;

/// 受声明区间约束的写通道。
pub struct WriteChannel<'a> {
    target: &'a mut dyn TargetHandle,
    declared: RegionSet,
    /// 构造时按值收下的令牌；通道持有它，调用方无法再用同一枚令牌开第二个通道。
    _token: Option<Recorded>,
    written: Vec<(Region, Digest)>,
    writes_done: usize,
    fail_after: Option<usize>,
}

impl<'a> WriteChannel<'a> {
    /// 以本步令牌与声明区间建立通道。
    ///
    /// 令牌按值交入并留在通道里：会话、步骤号、区间摘要三者都要与本次构造一致。
    /// 任何一项不符都说明令牌不属于这一步，通道不会建立。
    pub fn new(
        target: &'a mut dyn TargetHandle,
        session: SessionId,
        token: Recorded,
        step: u64,
        declared: &[Region],
    ) -> Result<Self, ExecError> {
        if token.session() != session {
            return Err(ExecError::InvalidPlan(
                "the write token was minted by a different record session".to_owned(),
            ));
        }
        if token.step() != step {
            return Err(ExecError::InvalidPlan(format!(
                "the write token was minted for step {}, not step {step}",
                token.step()
            )));
        }
        if token.region_digest() != RegionDigest::of(declared).finish() {
            return Err(ExecError::InvalidPlan(
                "the write token does not cover this step's declared regions".to_owned(),
            ));
        }
        Ok(Self {
            target,
            declared: RegionSet::from_regions(declared.iter().copied()),
            _token: Some(token),
            written: Vec::new(),
            writes_done: 0,
            fail_after: None,
        })
    }

    /// 恢复回滚专用：授权来自记录里的原像，而不是本步令牌。
    ///
    /// 这是记录事务之外唯一的写入口，也是第二写入通道。它绕过的只是"本步令牌
    /// 已铸造"这一步：能走到回滚，前提是记录里已有原像帧，而记录与锁在打开时
    /// 已核对过目标身份。逐条准入（区间是否仍落在当前目标内、内容摘要是否相符）
    /// 由恢复执行器负责。
    pub(crate) fn for_recovery(target: &'a mut dyn TargetHandle, declared: &[Region]) -> Self {
        Self {
            target,
            declared: RegionSet::from_regions(declared.iter().copied()),
            _token: None,
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
        read_exact_region(&*self.target, offset, len)
    }

    /// 写一段字节；区间必须被本步声明覆盖。
    pub fn write(&mut self, region: Region, bytes: &[u8]) -> Result<(), ExecError> {
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
        verify_digests(&*self.target, &self.written)
    }
}

/// 从目标读满 `len` 字节；读到 0 表示对端提前结束。
fn read_exact_region(
    target: &dyn TargetHandle,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, ExecError> {
    let mut buffer = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        let read = target
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

/// 逐区间回读并与期望摘要比对。
///
/// 只依赖目标句柄的读写，与目标上的文件系统无关：直写与载体写走同一条回读路径。
pub(crate) fn verify_digests(
    target: &dyn TargetHandle,
    expected: &[(Region, Digest)],
) -> Result<(), ExecError> {
    for (region, digest) in expected {
        let mut hasher = Hasher::new();
        let mut offset = region.start();
        let mut remaining = region.len();
        while remaining > 0 {
            let take = remaining.min(VERIFY_CHUNK as u64) as usize;
            let bytes = read_exact_region(target, offset, take)?;
            hasher.update(&bytes);
            offset += take as u64;
            remaining -= take as u64;
        }
        if hasher.finish() != *digest {
            return Err(ExecError::Postcondition(format!(
                "read-back digest mismatch for region [{}, {})",
                region.start(),
                region.end()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::app::execute::transaction::Mint;
    use crate::platform::{PlatformResult, TargetKind};

    /// 长度由外部持有的原子值决定；用来验证通道每次都按此刻长度判上界。
    struct ResizableTarget {
        bytes: Vec<u8>,
        len: Arc<AtomicU64>,
    }

    impl TargetHandle for ResizableTarget {
        fn path(&self) -> &Path {
            Path::new("resizable")
        }

        fn len(&self) -> u64 {
            self.len.load(Ordering::SeqCst)
        }

        fn kind(&self) -> TargetKind {
            TargetKind::File
        }

        fn sector_size(&self) -> Option<u64> {
            Some(512)
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
            let start = offset as usize;
            let visible = (self.len() as usize).min(self.bytes.len());
            let end = (start + buf.len()).min(visible);
            if start >= end {
                return Ok(0);
            }
            let n = end - start;
            buf[..n].copy_from_slice(&self.bytes[start..end]);
            Ok(n)
        }

        fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
            let start = offset as usize;
            let end = (start + buf.len()).min(self.bytes.len());
            if start >= end {
                return Ok(0);
            }
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

    fn token(session: SessionId, step: u64, declared: &[Region]) -> Recorded {
        Recorded::mint(&Mint::for_test(), session, step, declared)
    }

    fn channel<'a>(
        target: &'a mut MemoryTarget,
        session: SessionId,
        step: u64,
        declared: &[Region],
    ) -> Result<WriteChannel<'a>, ExecError> {
        WriteChannel::new(
            target,
            session,
            token(session, step, declared),
            step,
            declared,
        )
    }

    #[test]
    fn a_write_outside_the_declared_regions_is_rejected() {
        let mut target = MemoryTarget {
            bytes: vec![0u8; 4096],
        };
        let declared = [Region::sized(0, 512).expect("区间")];
        let mut channel = channel(&mut target, SessionId::next(), 0, &declared).expect("通道");
        let region = Region::sized(512, 512).expect("区间");
        let error = channel
            .write(region, &[0u8; 512])
            .expect_err("未声明的区间必须被拒绝");
        assert!(
            error.to_string().contains("not declared"),
            "实际错误：{error}"
        );
    }

    #[test]
    fn a_write_past_the_target_length_is_rejected() {
        let mut target = MemoryTarget {
            bytes: vec![0u8; 4096],
        };
        let declared = [Region::sized(0, 8192).expect("区间")];
        let mut channel = channel(&mut target, SessionId::next(), 0, &declared).expect("通道");
        let region = Region::sized(4096, 512).expect("区间");
        let error = channel
            .write(region, &[0u8; 512])
            .expect_err("越过目标长度的写入必须被拒绝");
        assert!(
            error.to_string().contains("past the target length"),
            "实际错误：{error}"
        );
    }

    #[test]
    fn a_declared_in_bounds_write_reads_back_identically() {
        let mut target = MemoryTarget {
            bytes: vec![0u8; 4096],
        };
        let declared = [Region::sized(0, 1024).expect("区间")];
        let mut channel = channel(&mut target, SessionId::next(), 0, &declared).expect("通道");
        let region = Region::sized(0, 512).expect("区间");
        channel
            .write(region, &[0x5au8; 512])
            .expect("区间内的写入应成功");
        channel.verify_written().expect("回读摘要应一致");
    }

    #[test]
    fn a_token_from_another_session_is_rejected() {
        let mut target = MemoryTarget {
            bytes: vec![0u8; 4096],
        };
        let declared = [Region::sized(0, 512).expect("区间")];
        let foreign = token(SessionId::next(), 0, &declared);
        let error = WriteChannel::new(&mut target, SessionId::next(), foreign, 0, &declared)
            .err()
            .expect("别的会话的令牌必须被拒绝");
        assert!(
            error.to_string().contains("different record session"),
            "实际错误：{error}"
        );
    }

    #[test]
    fn a_token_for_another_step_is_rejected() {
        let mut target = MemoryTarget {
            bytes: vec![0u8; 4096],
        };
        let declared = [Region::sized(0, 512).expect("区间")];
        let session = SessionId::next();
        let error = WriteChannel::new(
            &mut target,
            session,
            token(session, 2, &declared),
            3,
            &declared,
        )
        .err()
        .expect("别的步骤的令牌必须被拒绝");
        assert!(
            error.to_string().contains("minted for step 2"),
            "实际错误：{error}"
        );
    }

    #[test]
    fn a_token_for_another_region_set_is_rejected() {
        let mut target = MemoryTarget {
            bytes: vec![0u8; 4096],
        };
        let declared = [Region::sized(0, 512).expect("区间")];
        let other = [Region::sized(0, 1024).expect("区间")];
        let session = SessionId::next();
        let error = WriteChannel::new(
            &mut target,
            session,
            token(session, 0, &other),
            0,
            &declared,
        )
        .err()
        .expect("区间摘要不符的令牌必须被拒绝");
        assert!(
            error.to_string().contains("does not cover"),
            "实际错误：{error}"
        );
    }

    #[test]
    fn the_channel_checks_the_target_length_at_write_time() {
        let len = Arc::new(AtomicU64::new(4096));
        let mut target = ResizableTarget {
            bytes: vec![0u8; 4096],
            len: Arc::clone(&len),
        };
        let declared = [Region::sized(0, 2048).expect("区间")];
        let session = SessionId::next();
        let mut channel = WriteChannel::new(
            &mut target,
            session,
            token(session, 0, &declared),
            0,
            &declared,
        )
        .expect("通道");
        assert_eq!(channel.target_len(), 4096);
        // 通道建立后目标被外部截断：上界必须按此刻长度判，而不是建立时的缓存。
        len.store(1024, Ordering::SeqCst);
        assert_eq!(channel.target_len(), 1024);
        let region = Region::sized(0, 2048).expect("区间");
        let error = channel
            .write(region, &[0u8; 2048])
            .expect_err("按此刻长度已越界，必须拒绝");
        assert!(
            error.to_string().contains("past the target length"),
            "实际错误：{error}"
        );
    }

    #[test]
    fn a_read_back_that_differs_from_the_written_bytes_is_reported() {
        let mut target = MemoryTarget {
            bytes: vec![0u8; 4096],
        };
        let declared = [Region::sized(0, 512).expect("区间")];
        let region = Region::sized(0, 512).expect("区间");
        {
            let mut channel = channel(&mut target, SessionId::next(), 0, &declared).expect("通道");
            channel.write(region, &[0x5au8; 512]).expect("写入");
        }
        // 期望摘要取另一段字节：读回来的内容与之不符，必须报错。
        let mut hasher = Hasher::new();
        hasher.update(&[0x11u8; 512]);
        let error = verify_digests(&target, &[(region, hasher.finish())])
            .expect_err("回读与期望不符必须被报告");
        assert!(error.to_string().contains("mismatch"), "实际错误：{error}");
    }

    #[test]
    fn a_read_back_past_the_current_length_ends_early() {
        let len = Arc::new(AtomicU64::new(4096));
        let mut target = ResizableTarget {
            bytes: vec![0u8; 4096],
            len: Arc::clone(&len),
        };
        let declared = [Region::sized(0, 2048).expect("区间")];
        let region = Region::sized(0, 2048).expect("区间");
        let session = SessionId::next();
        let mut channel = WriteChannel::new(
            &mut target,
            session,
            token(session, 0, &declared),
            0,
            &declared,
        )
        .expect("通道");
        channel.write(region, &[0x5au8; 2048]).expect("写入");
        // 目标被外部截断到一扇区：回读越界，读取提前结束。
        len.store(512, Ordering::SeqCst);
        let error = channel
            .verify_written()
            .expect_err("回读不到声明长度必须失败");
        assert!(
            error.to_string().contains("ended early"),
            "实际错误：{error}"
        );
    }
}
