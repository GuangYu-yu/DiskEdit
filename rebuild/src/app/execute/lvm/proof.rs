//! 物理卷缩容的前置证明。
//!
//! 缩容后的长度必须仍容纳元数据区（`pe_start` 之前的部分）；否则一律拒绝，
//! 不依赖外部工具的失败作为保护。

use crate::platform::PvFacts;

/// 缩容证明不成立的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PvRefusal {
    /// 给出的长度不是缩容。
    NotAShrink { current: u64 },
    /// 给出的长度落进元数据区。
    BelowMetadata { pe_start: u64 },
    /// 长度为零。
    Empty,
}

/// 证明把物理卷缩到 `new_size_bytes` 是安全的。
pub fn prove_shrink(row: &PvFacts, new_size_bytes: u64) -> Result<(), PvRefusal> {
    if new_size_bytes == 0 {
        return Err(PvRefusal::Empty);
    }
    if new_size_bytes >= row.size_bytes {
        return Err(PvRefusal::NotAShrink {
            current: row.size_bytes,
        });
    }
    if new_size_bytes < row.pe_start_bytes {
        return Err(PvRefusal::BelowMetadata {
            pe_start: row.pe_start_bytes,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> PvFacts {
        PvFacts {
            name: "sda2".to_owned(),
            size_bytes: 8 * 1024 * 1024,
            pe_start_bytes: 1024 * 1024,
            vg_name: "vg0".to_owned(),
        }
    }

    #[test]
    fn shrink_above_metadata_is_allowed() {
        assert!(prove_shrink(&row(), 4 * 1024 * 1024).is_ok());
        assert!(prove_shrink(&row(), 1024 * 1024).is_ok());
    }

    #[test]
    fn grow_and_equal_are_refused() {
        assert_eq!(
            prove_shrink(&row(), 9 * 1024 * 1024),
            Err(PvRefusal::NotAShrink {
                current: 8 * 1024 * 1024
            })
        );
        assert!(matches!(
            prove_shrink(&row(), 8 * 1024 * 1024),
            Err(PvRefusal::NotAShrink { .. })
        ));
    }

    #[test]
    fn into_metadata_is_refused() {
        assert_eq!(
            prove_shrink(&row(), 512 * 1024),
            Err(PvRefusal::BelowMetadata {
                pe_start: 1024 * 1024
            })
        );
        assert_eq!(prove_shrink(&row(), 0), Err(PvRefusal::Empty));
    }
}
