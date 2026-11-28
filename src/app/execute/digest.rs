//! 区域摘要：对写入前后的字节做稳定的 128 位摘要。
//!
//! 摘要只用于核对"读回来的字节是否等于写下去的字节"和记录里的原像标识，
//! 不作为安全边界；实现是两条独立的 64 位流，允许增量喂入，避免把整段区域
//! 读进内存。

use diskedit_core::region::Region;

use super::ExecError;
use crate::app::decide::digest::{FNV_OFFSET_BASIS, FNV_PRIME};
use crate::platform::TargetHandle;

/// 128 位区域摘要。
pub type Digest = [u8; 16];

/// 右通道的混入乘数：黄金比例在 2^64 下的取整。
const MIX: u64 = 0x9e37_79b9_7f4a_7c15;

/// 增量摘要器。
#[derive(Clone, Debug)]
pub struct Hasher {
    left: u64,
    right: u64,
    fed: u64,
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher {
    pub const fn new() -> Self {
        Self {
            left: FNV_OFFSET_BASIS,
            right: MIX,
            fed: 0,
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.left ^= u64::from(*byte);
            self.left = self.left.wrapping_mul(FNV_PRIME);
            self.right = self
                .right
                .rotate_left(7)
                .wrapping_add(u64::from(*byte))
                .wrapping_mul(MIX);
            self.fed = self.fed.wrapping_add(1);
        }
    }

    pub fn feed_u64(&mut self, value: u64) {
        self.update(&value.to_le_bytes());
    }

    pub fn finish(&self) -> Digest {
        let mut digest = [0u8; 16];
        let left = self.left ^ self.fed.rotate_left(17);
        let right = self.right ^ self.fed.wrapping_mul(0x2545f4914f6cdd1d);
        digest[..8].copy_from_slice(&left.to_le_bytes());
        digest[8..].copy_from_slice(&right.to_le_bytes());
        digest
    }
}

/// 一次性摘要。
pub fn digest(bytes: &[u8]) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    hasher.finish()
}

/// 读一段区域并增量摘要；按块读取，单次缓冲不超过 `chunk`。
///
/// 摘要只覆盖读满 `region.len()` 之后才计算：任何一次读少于请求长度都按 IO 失败
/// 返回，不把残缺字节当成区域内容。
pub fn hash_region(
    target: &mut dyn TargetHandle,
    region: Region,
    chunk: usize,
) -> Result<Digest, ExecError> {
    let mut hasher = Hasher::new();
    let mut offset = region.start();
    let mut remaining = region.len();
    let mut buffer = vec![0u8; chunk.max(1)];
    while remaining > 0 {
        let take = remaining.min(buffer.len() as u64) as usize;
        let read = target
            .read_at(offset, &mut buffer[..take])
            .map_err(ExecError::from)?;
        if read != take {
            return Err(ExecError::Io(format!(
                "read of region ended early: expected {take} bytes, got {read} bytes"
            )));
        }
        hasher.update(&buffer[..take]);
        offset += take as u64;
        remaining -= take as u64;
    }
    Ok(hasher.finish())
}

/// 读一段区域的原始字节；用于表区原像。
///
/// 表区远小于数据区，整段读入是安全的。读取不足时按 IO 失败处理，不返回残缺
/// 原像。
pub fn read_region(
    target: &mut dyn TargetHandle,
    region: Region,
    chunk: usize,
) -> Result<Vec<u8>, ExecError> {
    let len = usize::try_from(region.len()).map_err(|_| {
        ExecError::Io("region length exceeds the range representable on this host".to_owned())
    })?;
    let mut buffer = vec![0u8; len];
    let mut offset = region.start();
    let mut filled = 0;
    while filled < len {
        let take = (len - filled).min(chunk.max(1));
        let read = target
            .read_at(offset, &mut buffer[filled..filled + take])
            .map_err(ExecError::from)?;
        if read != take {
            return Err(ExecError::Io(format!(
                "read of region ended early: expected {take} bytes, got {read} bytes"
            )));
        }
        filled += take;
        offset += take as u64;
    }
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::platform::{PlatformResult, TargetKind};

    /// 每次只吐出至多 `per_read` 字节的目标；用来注入短读。
    struct ShortReader {
        bytes: Vec<u8>,
        per_read: usize,
    }

    impl TargetHandle for ShortReader {
        fn path(&self) -> &Path {
            Path::new("short-reader")
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
            if start >= self.bytes.len() {
                return Ok(0);
            }
            let end = (start + buf.len()).min(self.bytes.len());
            let n = (end - start).min(self.per_read);
            buf[..n].copy_from_slice(&self.bytes[start..start + n]);
            Ok(n)
        }

        fn write_at(&mut self, _offset: u64, buf: &[u8]) -> PlatformResult<usize> {
            Ok(buf.len())
        }

        fn barrier(&mut self) -> PlatformResult<()> {
            Ok(())
        }

        fn refresh_partitions(&mut self) -> PlatformResult<()> {
            Ok(())
        }
    }

    #[test]
    fn same_bytes_same_digest_different_bytes_different_digest() {
        assert_eq!(digest(b"abc"), digest(b"abc"));
        assert_ne!(digest(b"abc"), digest(b"abd"));
        assert_ne!(digest(b""), digest(b"\0"));
    }

    #[test]
    fn incremental_matches_one_shot() {
        let mut hasher = Hasher::new();
        hasher.update(b"ab");
        hasher.update(b"c");
        assert_eq!(hasher.finish(), digest(b"abc"));
    }

    #[test]
    fn a_short_read_of_a_region_is_not_hashed_as_the_region() {
        let bytes = vec![0x5au8; 4096];
        let mut short = ShortReader {
            bytes: bytes.clone(),
            per_read: 100,
        };
        let region = Region::sized(0, 4096).expect("区间");
        let error = hash_region(&mut short, region, 1 << 20)
            .expect_err("短读必须失败，而不是对残缺字节求摘要");
        assert!(error.to_string().contains("ended early"), "{error}");

        let mut full = ShortReader {
            bytes,
            per_read: usize::MAX,
        };
        assert_eq!(
            hash_region(&mut full, region, 1 << 20).expect("读满可求摘要"),
            digest(&[0x5au8; 4096])
        );
    }
}
