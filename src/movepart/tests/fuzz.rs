//! 固定种子、无外部依赖的健壮性回归：块布局算式。

use crate::planner::chunk::{move_chunk_layout, relocation_overlaps};
use crate::testrng::Rng;

const LAYOUT_SEED: u64 = 0x5EED_0000_0004;
const LAYOUT_ROUNDS: u32 = 3000;

/// 块布局算式：任何输入都不得 panic；返回的布局必须可覆盖全部待拷贝字节、
/// 单块不超过请求块大小，且自重叠搬移时单块不超过位移（否则本次写会覆盖未读源区）
#[test]
fn move_chunk_layout_invariants_fixed_seed() {
    let seed = LAYOUT_SEED;
    let mut rng = Rng::new(seed);
    let pool: [u64; 12] = [0, 1, 2, 3, 511, 512, 513, 1024, 1 << 20, 1 << 31, u64::MAX - 1, u64::MAX];
    for round in 0..LAYOUT_ROUNDS {
        let copy_lba = rng.pick(&pool);
        let ss = rng.pick(&[1u64, 512, 4096]);
        let shift_lba = rng.pick(&pool);
        let src_len_lba = rng.pick(&pool);
        let chunk_bytes = rng.pick(&pool);
        let ctx = format!("seed={seed} round={round} copy={copy_lba} ss={ss} shift={shift_lba} src_len={src_len_lba} chunk_bytes={chunk_bytes}");
        let got = move_chunk_layout(copy_lba, ss, shift_lba, src_len_lba, chunk_bytes);
        let overlap = relocation_overlaps(shift_lba, src_len_lba);
        let shift_bytes = shift_lba.checked_mul(ss);
        let bytes = copy_lba.checked_mul(ss);
        let effective = if overlap {
            shift_bytes.map_or(0, |b| chunk_bytes.min(b))
        } else {
            chunk_bytes
        };
        match got {
            Some((chunk, count)) => {
                let total = bytes.unwrap_or_else(|| panic!("{ctx}: a representable layout implies the byte count fits"));
                assert!(chunk > 0, "{ctx}: returned a zero-sized chunk");
                assert!(chunk <= chunk_bytes, "{ctx}: a chunk grew beyond the requested size");
                if overlap {
                    assert!(
                        chunk <= shift_bytes.unwrap(),
                        "{ctx}: an overlapping relocation may write past its own shift"
                    );
                }
                assert_eq!(count, total.div_ceil(chunk), "{ctx}: block count is not the exact ceiling");
                // 待拷贝字节为 0 时没有块要做，count = 0 是唯一自洽值（生产入口的 copy 恒 ≥ 1）
                assert!(count > 0 || total == 0, "{ctx}: a zero block count with bytes to copy");
                if count > 0 {
                    assert!((count - 1) * chunk < total, "{ctx}: the last block starts at or past the end");
                    assert!(count * chunk >= total, "{ctx}: layout does not cover the copied bytes");
                    assert!(count * chunk - total < chunk, "{ctx}: layout overshoots by more than one block");
                }
            }
            None => {
                let unrepresentable = bytes.is_none()
                    || effective == 0
                    || bytes.is_some_and(|b| b.checked_add(effective - 1).is_none());
                assert!(unrepresentable, "{ctx}: a representable layout was reported as absent");
            }
        }
    }
}
