//! 固定种子、无外部依赖的健壮性回归：resize checkpoint 反序列化与块布局算式。
//!
//! 断言不是统一的"只允许拒绝"：合法输入必须往返一致，畸形输入才验证安全拒绝。

use super::support::*;
use super::*;
use crate::testrng::Rng;

const CKPT_SEED: u64 = 0x5EED_0000_0003;
const CKPT_ROUNDS: u32 = 1500;
const LAYOUT_SEED: u64 = 0x5EED_0000_0004;
const LAYOUT_ROUNDS: u32 = 3000;

/// 合法 checkpoint 的生成：所有字段都从"可操作几何"的边界内取值，
/// 进度上界由唯一算式 `move_chunk_layout` 给出（测试不另写一套）
fn valid_ckpt(rng: &mut Rng, entry_count: u32) -> RsCheckpoint<crate::dev::LiveProof> {
    let ss = 512u64;
    let first = 34u64;
    let last = 200_000u64;
    let part = 1 + rng.below(entry_count as u64) as u32;
    // 起点落在可用区内，长度从 1 起：保证 start <= end 且两个端点都在区间内
    let old_start = first + rng.below(last - first + 1);
    let old_end = old_start + rng.below(last - old_start + 1);
    let new_start = first + rng.below(last - first + 1);
    let new_end = new_start + rng.below(last - new_start + 1);
    let moved = old_start.abs_diff(new_start);
    let old_len = old_end - old_start + 1;
    let copy_len = old_len.min(new_end - new_start + 1);
    let chunk_pool: [u64; 6] = [1, 2, 512, 1024, 1 << 20, u32::MAX as u64];
    let mut chunk_bytes = rng.pick(&chunk_pool);
    let count = loop {
        match move_chunk_layout(copy_len, ss, moved, old_len, chunk_bytes) {
            Some((_, n)) => break n,
            // 取值池里只有 chunk_bytes 可能让布局不存在（位移/长度都不为 0）
            None => chunk_bytes = 512,
        }
    };
    let chunks_done = rng.below(count + 1);
    let fs_shrunk = rng.pick(&[FsShrink::NotShrunk, FsShrink::Verified, FsShrink::Unverified]);
    let fstype = rng.pick(&[
        None,
        Some(crate::fsid::FsKind::Ext),
        Some(crate::fsid::FsKind::Xfs),
        Some(crate::fsid::FsKind::Ntfs),
    ]);
    let proof = rng.pick(&[crate::dev::LiveProof::DeviceId, crate::dev::LiveProof::Inode, crate::dev::LiveProof::Unavailable]);
    let map = if rng.bool() { Some((rng.below(1 << 20), rng.below(1 << 20))) } else { None };
    RsCheckpoint {
        proof,
        disk_size: 1 << 40,
        ss,
        part,
        old_start,
        old_end,
        new_start,
        new_end,
        fs_shrunk,
        chunks_done,
        chunk_bytes,
        fp: crate::dev::TargetFingerprint {
            dev: rng.u64(),
            ino: rng.u64(),
            size: rng.u64(),
            id: rng.u64(),
        },
        map,
        fstype,
        lvm: None,
    }
}

/// 反序列化结果的字段不变量：任何被接受的现场都必须落在可操作几何内
fn check_ckpt(c: &RsCheckpoint<crate::dev::IdentityProof>, lim: &GeometryLimits, ctx: &str) {
    assert_eq!(c.ss, lim.sector_size, "{ctx}: accepted a checkpoint under another sector size");
    assert!((1..=lim.entry_count).contains(&c.part), "{ctx}: accepted an out-of-range partition number");
    assert!(c.old_start <= c.old_end && c.new_start <= c.new_end, "{ctx}: accepted inverted range endpoints");
    assert!(
        c.old_start >= lim.first_usable_lba && c.old_end <= lim.last_usable_lba,
        "{ctx}: accepted a source range outside the usable area"
    );
    assert!(
        c.new_start >= lim.first_usable_lba && c.new_end <= lim.last_usable_lba,
        "{ctx}: accepted a target range outside the usable area"
    );
    assert!(c.chunk_bytes > 0, "{ctx}: accepted a zero chunk size");
    let moved = c.old_start.abs_diff(c.new_start);
    let old_len = c.old_end - c.old_start + 1;
    let copy_len = old_len.min(c.new_end - c.new_start + 1);
    let limit = move_chunk_layout(copy_len, c.ss, moved, old_len, c.chunk_bytes)
        .map(|(_, n)| n)
        .expect("an accepted checkpoint must have a representable block layout");
    assert!(c.chunks_done <= limit, "{ctx}: accepted chunk progress beyond the block count");
}

/// 合法输入：序列化 → 反序列化必须逐字段一致（跨文件路径、loop 映射、FS 类型与身份种类）
#[test]
fn rs_checkpoint_round_trips_valid_state_fixed_seed() {
    let seed = CKPT_SEED;
    let mut rng = Rng::new(seed);
    for round in 0..CKPT_ROUNDS {
        let entry_count = rng.pick(&[1u32, 4, 8, 128]);
        let ck = valid_ckpt(&mut rng, entry_count);
        let bytes = ck.serialize();
        let lim = lim(entry_count);
        let ctx = format!("seed={seed} round={round} part={} range={}..{}/{}..{}", ck.part, ck.old_start, ck.old_end, ck.new_start, ck.new_end);
        let back = RsCheckpoint::<crate::dev::IdentityProof>::deserialize(&bytes, &lim)
            .unwrap_or_else(|e| panic!("{ctx}: a legal checkpoint failed to read back: {e}"));
        check_ckpt(&back, &lim, &ctx);
        assert_eq!(back.disk_size, ck.disk_size, "{ctx}: disk size changed");
        assert_eq!((back.ss, back.part), (ck.ss, ck.part), "{ctx}: geometry changed");
        assert_eq!((back.old_start, back.old_end), (ck.old_start, ck.old_end), "{ctx}: source range changed");
        assert_eq!((back.new_start, back.new_end), (ck.new_start, ck.new_end), "{ctx}: target range changed");
        assert_eq!(back.fs_shrunk, ck.fs_shrunk, "{ctx}: shrink state changed");
        assert_eq!((back.chunks_done, back.chunk_bytes), (ck.chunks_done, ck.chunk_bytes), "{ctx}: progress changed");
        assert_eq!(back.map, ck.map, "{ctx}: loop mapping changed");
        assert_eq!(back.fstype, ck.fstype, "{ctx}: filesystem kind changed");
        assert_eq!(
            (back.fp.dev, back.fp.ino, back.fp.size, back.fp.id),
            (ck.fp.dev, ck.fp.ino, ck.fp.size, ck.fp.id),
            "{ctx}: fingerprint changed"
        );
        assert!(
            matches!(back.proof, crate::dev::IdentityProof::Live(p) if p == ck.proof),
            "{ctx}: identity kind changed"
        );
    }
}

/// 畸形输入：任意截断、任意字节改写、以及"改完再补 CRC"的字段改写都不得 panic；
/// 被接受的必须过 `check_ckpt`，被拒绝的不要求具体错误码（CRC/字段校验任一挡住即可）
#[test]
fn rs_checkpoint_rejects_corrupt_state_fixed_seed() {
    let seed = CKPT_SEED;
    let mut rng = Rng::new(seed);
    let entry_count = 128u32;
    let lim = lim(entry_count);
    let base = valid_ckpt(&mut Rng::new(seed ^ 0xABCD), entry_count);
    let base = base.serialize();
    let body = base.len() - 4;
    for round in 0..CKPT_ROUNDS {
        let mut bytes = base.clone();
        let ctx = format!("seed={seed} round={round} base_len={}", base.len());
        match rng.below(4) {
            // 1) 严格前缀：任何短于原长的输入都缺了尾部字段或 CRC，必须拒绝
            0 => {
                let keep = rng.below(base.len() as u64) as usize;
                bytes.truncate(keep);
                let ctx = format!("{ctx} truncate_to={keep}");
                assert!(
                    RsCheckpoint::<crate::dev::IdentityProof>::deserialize(&bytes, &lim).is_err(),
                    "{ctx}: a truncated checkpoint must never be accepted"
                );
                continue;
            }
            // 2) 任意字节改写（含 CRC 区）
            1 => {
                let n = 1 + rng.below(4);
                for _ in 0..n {
                    let p = rng.below(bytes.len() as u64) as usize;
                    bytes[p] ^= 1 << rng.below(8);
                }
            }
            // 3) 字段区改写但不补 CRC：CRC 必须挡住；逐字节取反保证内容确实变了
            2 => {
                let p = rng.below(body as u64) as usize;
                bytes[p] ^= 1 << rng.below(8);
                assert!(
                    RsCheckpoint::<crate::dev::IdentityProof>::deserialize(&bytes, &lim).is_err(),
                    "{ctx} byte={p}: a field rewrite without a fresh CRC must be rejected by the CRC gate"
                );
                continue;
            }
            // 4) 字段区改写后按规范补 CRC：此时挡住它的只能是字段校验本身
            _ => {
                let p = rng.below(body as u64) as usize;
                let pool: [u64; 7] = [0, 1, 2, u32::MAX as u64, u64::MAX, 200_001, 34];
                let v = rng.pick(&pool).to_le_bytes();
                let end = (p + 1 + rng.below(8) as usize).min(body);
                bytes[p..end].copy_from_slice(&v[..end - p]);
                let crc = crate::table::crc32(&bytes[..body]);
                bytes[body..].copy_from_slice(&crc.to_le_bytes());
            }
        }
        if let Ok(c) = RsCheckpoint::<crate::dev::IdentityProof>::deserialize(&bytes, &lim) {
            check_ckpt(&c, &lim, &ctx);
        }
    }
}

/// 已知边界的最小复现样本保留为普通用例：版本不对、chunk 为 0、区间倒挂都必须拒绝，
/// 且拒绝来自字段校验（这些样本的 CRC 都按规范重算过）
#[test]
fn rs_checkpoint_field_guards_are_not_crc_artifacts() {
    let entry_count = 128u32;
    let lim = lim(entry_count);
    let base = valid_ckpt(&mut Rng::new(CKPT_SEED ^ 0xABCD), entry_count).serialize();
    let body = base.len() - 4;
    let recalc = |mut b: Vec<u8>| {
        let crc = crate::table::crc32(&b[..body]);
        b[body..].copy_from_slice(&crc.to_le_bytes());
        b
    };
    let deser = |b: &[u8]| RsCheckpoint::<crate::dev::IdentityProof>::deserialize(b, &lim);

    // 版本号：8..12
    let version = u32::from_le_bytes(base[8..12].try_into().unwrap());
    let mut b = base.clone();
    b[8..12].copy_from_slice(&version.wrapping_add(1).to_le_bytes());
    let b = recalc(b);
    assert!(deser(&b).is_err(), "an unsupported version must be refused");

    // chunk_bytes：73..81（对应 serialize 的字段顺序）
    let mut b = base.clone();
    b[73..81].copy_from_slice(&0u64.to_le_bytes());
    let b = recalc(b);
    assert!(deser(&b).is_err(), "chunk_bytes = 0 must be refused");

    // old_start > old_end：32..48
    let mut b = base.clone();
    b[32..40].copy_from_slice(&100u64.to_le_bytes());
    b[40..48].copy_from_slice(&99u64.to_le_bytes());
    let b = recalc(b);
    assert!(deser(&b).is_err(), "inverted range endpoints must be refused");
}

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
