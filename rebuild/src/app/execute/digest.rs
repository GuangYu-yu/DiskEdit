//! 区域摘要：对写入前后的字节做稳定的 128 位摘要。
//!
//! 摘要只用于核对"读回来的字节是否等于写下去的字节"和记录里的原像标识，
//! 不作为安全边界；实现是两条独立的 64 位流，允许增量喂入，避免把整段区域
//! 读进内存。

use diskedit_core::region::Region;

use super::ExecError;
use crate::platform::TargetHandle;

/// 128 位区域摘要。
pub type Digest = [u8; 16];

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
            left: 0xcbf29ce484222325,
            right: 0x9e3779b97f4a7c15,
            fed: 0,
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.left ^= u64::from(*byte);
            self.left = self.left.wrapping_mul(0x100000001b3);
            self.right = self
                .right
                .rotate_left(7)
                .wrapping_add(u64::from(*byte))
                .wrapping_mul(0x9e3779b97f4a7c15);
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
        target
            .read_at(offset, &mut buffer[..take])
            .map_err(ExecError::from)?;
        hasher.update(&buffer[..take]);
        offset += take as u64;
        remaining -= take as u64;
    }
    Ok(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
