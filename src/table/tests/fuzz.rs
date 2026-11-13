#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
//! 固定种子、无外部依赖的健壮性回归：MBR / GPT 解析。
//!
//! 生成策略：每个用例从"边界值池"取值（0、1、最大整数、扇区边界、容器末端），
//! 由 [`Rng`] 负责组合；随机只用于覆盖组合空间，不代替定向用例。
//! 断言分两类且不可互换：合法输入验证结果正确，畸形输入验证安全拒绝。

use super::support::*;
use super::*;
use crate::testrng::Rng;

const MBR_SEED: u64 = 0x5EED_0000_0001;
const MBR_ROUNDS: u32 = 1500;
const GPT_SEED: u64 = 0x5EED_0000_0002;
const GPT_ROUNDS: u32 = 1000;
const SS: u64 = 512;

/// 每轮一个独立临时目录：路径不撞、不受上一轮残留影响，目录在 `FileSource` 析构后
/// 随 `TestDir` 一起删除
fn open_src(tag: &str, data: &[u8]) -> crate::testdir::TestSource<FileSource> {
    let dir = crate::testdir::test_dir(tag);
    let p = dir.join("fuzz.img");
    std::fs::write(&p, data).unwrap();
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&p).unwrap();
    let src = FileSource {
        identity: crate::dev::TargetIdentity::resolve_image(&p),
        block_identity: None,
        file: f,
        path: p,
        sector_size: SS,
        size: data.len() as u64,
        is_block: false,
        record_journal: None,
        fingerprint: Default::default(),
        loop_mapping: None,
        owner: None,
    };
    crate::testdir::TestSource::new(dir, src)
}

/// 固定种子的 MBR 输入：LBA0 的四个槽位按池取值（类型/起点/长度都含 0、
/// 容器首尾、u32 上界），签名一半在场，LBA1 保持全零（GptHeader 不构成第二条盘型判据）
#[test]
fn parse_mbr_raw_fuzz_fixed_seed() {
    let seed = MBR_SEED;
    let mut rng = Rng::new(seed);
    let total_pool: [u64; 6] = [1, 2, 3, 68, 200, 4097];
    for round in 0..MBR_ROUNDS {
        let total = rng.pick(&total_pool);
        let mut data = vec![0u8; (total * SS) as usize];
        let sig = rng.bool();
        if sig {
            data[510] = 0x55;
            data[511] = 0xAA;
        }
        let t = total as u32;
        let mut written = Vec::new();
        for i in 0..4usize {
            let off = 446 + i * 16;
            let ty = rng.pick(&[0u8, 0x01, 0x05, 0x0C, 0x83, 0xEE, 0xFF]);
            let start = rng.pick(&[0u32, 1, 2, t.saturating_sub(1), t, t.saturating_add(1), u32::MAX]);
            let size = rng.pick(&[0u32, 1, 2, 4, t, u32::MAX]);
            data[off + 4] = ty;
            data[off + 8..off + 12].copy_from_slice(&start.to_le_bytes());
            data[off + 12..off + 16].copy_from_slice(&size.to_le_bytes());
            written.push((ty, start, size));
        }
        let ctx = format!("seed={seed} round={round} total_sectors={total} signature={sig} entries={written:?}");
        let src = open_src("mbr", &data);

        // 任意字节都不 panic，且结果来自可解释的判型分支
        let raw = match parse_mbr_raw(&src) {
            Ok(r) => r,
            Err(e) => panic!("{ctx}: parse_mbr_raw reported an io error on a well-formed file: {e}"),
        };
        let Some(r) = raw else {
            // None 只有两个合法来源：签名缺席，或 LBA0 本身是一份完好的保护 MBR
            assert!(!sig || pmbr_shape_valid(&src).unwrap(), "{ctx}: an MBR-shaped LBA0 vanished without a protective layout");
            continue;
        };

        // 合法输入的观测结果必须与盘上字节一致
        for p in &r.parts {
            assert!((1..=4).contains(&p.num), "{ctx}: partition number out of range");
            assert_ne!(p.os_type, 0, "{ctx}: an empty record must not be reported as a partition");
            assert_ne!(p.os_type, PROT_MBR_TYPE, "{ctx}: the protective record must not be reported as a partition");
            assert_ne!(p.size_lba, 0, "{ctx}: a zero-length record must not be reported");
            let start_zero = r.damage.contains(&MbrDamage::StartZero { num: p.num });
            assert_eq!(start_zero, p.start_lba == 0, "{ctx}: StartZero must be flagged exactly for LBA 0");
            let past = r
                .damage
                .iter()
                .any(|d| matches!(d, MbrDamage::PastEnd { num, .. } if *num == p.num));
            assert_eq!(
                past,
                p.start_lba as u64 + p.size_lba as u64 > total,
                "{ctx}: PastEnd must be flagged exactly when the entry leaves the disk"
            );
        }
        for i in 0..r.parts.len() {
            for j in i + 1..r.parts.len() {
                let (a, b) = (&r.parts[i], &r.parts[j]);
                let ae = a.start_lba as u64 + a.size_lba as u64 - 1;
                let be = b.start_lba as u64 + b.size_lba as u64 - 1;
                let overlap = !(ae < b.start_lba as u64 || be < a.start_lba as u64);
                let flagged = r
                    .damage
                    .iter()
                    .any(|d| matches!(d, MbrDamage::Overlap { num_a, num_b, .. } if *num_a == a.num && *num_b == b.num));
                assert_eq!(flagged, overlap, "{ctx}: Overlap must be flagged exactly for intersecting pairs");
            }
        }

        // 可操作性：有损伤的表必须被拒绝，无损伤的表必须与 raw 逐字段一致
        if r.damage.is_empty() {
            let parts = parse_mbr(&src).unwrap().expect("an undamaged table must stay operable");
            assert_eq!(parts.len(), r.parts.len(), "{ctx}: the validated reader dropped entries");
            for (a, b) in parts.iter().zip(&r.parts) {
                assert_eq!((a.num, a.os_type, a.start_lba, a.size_lba), (b.num, b.os_type, b.start_lba, b.size_lba), "{ctx}: the two readers disagree");
            }
        } else {
            let e = parse_mbr(&src).expect_err("a damaged table must be refused by the validated reader");
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{ctx}: a damaged table failed on the wrong channel: {e}");
        }
    }
}

/// 后备夹具：gptman 写出的合法 GPT（保护 MBR + 主副本 + 盘尾备份），读回成字节数组。
/// 标签带自增序号：本函数被两个 GPT 用例同时调用，同名临时路径会让一方删掉另一方
/// 正在读的文件
fn gpt_fixture() -> Vec<u8> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let tag = format!("fuzz_gpt_base_{}", N.fetch_add(1, Ordering::Relaxed));
    let src = src_from_gpt(&tag, SS);
    let path = src.path.clone();
    let data = std::fs::read(&path).unwrap();
    drop(src);
    let _ = std::fs::remove_file(&path);
    data
}

/// 按规范重算头 CRC（HeaderSize 字节、CRC 字段置零）。
/// 拒绝路径之外的用例靠它让"其他字段"的变化通过 CRC 这一关
fn fix_header_crc(sec: &mut [u8]) {
    let hs = u32::from_le_bytes(sec[12..16].try_into().unwrap()) as usize;
    let hs = hs.clamp(92, sec.len());
    sec[16..20].fill(0);
    let crc = crc32(&sec[..hs]);
    sec[16..20].copy_from_slice(&crc.to_le_bytes());
}

/// 让 `CopyDamaged` 之外的字段变化走到几何/条目校验：备份副本不在断言范围，
/// 故直接问"这一份副本"而不问 load_gpt 的主备决策
fn check_copy(copy: ParsedCopy, ctx: &str) {
    let ParsedCopy::Usable(g) = copy else { return };
    assert!(g.header.first_usable_lba <= g.header.last_usable_lba, "{ctx}: accepted a header with inverted usable range");
    assert!(g.header.partition_entry_lba >= 2, "{ctx}: accepted an entry array that covers LBA0/LBA1");
    assert_eq!(
        g.entries.len(),
        g.header.number_of_partition_entries as usize,
        "{ctx}: accepted a table whose entry count disagrees with its header"
    );
    for (i, e) in g.entries.iter().enumerate() {
        if e.starting_lba == 0 && e.ending_lba == 0 {
            continue;
        }
        assert!(e.starting_lba <= e.ending_lba, "{ctx}: accepted entry {} with start > end", i + 1);
        assert!(
            e.starting_lba >= g.header.first_usable_lba && e.ending_lba <= g.header.last_usable_lba,
            "{ctx}: accepted entry {} outside the usable range",
            i + 1
        );
    }
}

/// 随机改动头字段 / 头尺寸 / 条目字节（必要时同步修 CRC）与随机截断：
/// 任何被这一份副本接受的表都必须满足自述几何约束，绝不能把越界字段当事实带出去
#[test]
fn gpt_header_fuzz_fixed_seed() {
    let seed = GPT_SEED;
    let base = gpt_fixture();
    let mut rng = Rng::new(seed);
    let total = (base.len() as u64) / SS;
    for round in 0..GPT_ROUNDS {
        let mut data = base.clone();
        let kind = rng.below(6);
        let hdr = SS as usize;
        let mut desc = String::new();
        match kind {
            0 => {}
            1 => {
                let keep = 1 + rng.below(total);
                data.truncate((keep * SS) as usize);
                desc = format!("truncate_to_sectors={keep}");
            }
            2 => {
                let fields: [usize; 5] = [24, 32, 40, 48, 72];
                let off = rng.pick(&fields);
                let v = rng.pick(&[0u64, 1, 2, 3, total, total + 1, u64::MAX]);
                data[hdr + off..hdr + off + 8].copy_from_slice(&v.to_le_bytes());
                fix_header_crc(&mut data[hdr..hdr + SS as usize]);
                desc = format!("header_field@{off}={v}");
            }
            3 => {
                let hs = rng.pick(&[0u32, 1, 91, 92, 93, 512, 513, u32::MAX]);
                data[hdr + 12..hdr + 16].copy_from_slice(&hs.to_le_bytes());
                fix_header_crc(&mut data[hdr..hdr + SS as usize]);
                desc = format!("header_size={hs}");
            }
            4 => {
                let n = 1 + rng.below(8);
                for _ in 0..n {
                    let p = rng.below(data.len() as u64) as usize;
                    data[p] ^= 1 << rng.below(8);
                }
                desc = format!("flips={n}");
            }
            _ => {
                // 条目字段 + 数组 CRC + 头 CRC 同步改：几何/条目校验必须自己拦住越界值
                let n = u32::from_le_bytes(data[hdr + 80..hdr + 84].try_into().unwrap()) as usize;
                let es = u32::from_le_bytes(data[hdr + 84..hdr + 88].try_into().unwrap()) as usize;
                let ent_lba = u64::from_le_bytes(data[hdr + 72..hdr + 80].try_into().unwrap()) as usize;
                let array_off = ent_lba * SS as usize;
                let array_len = n * es;
                let idx = rng.below(n as u64) as usize;
                let field = rng.pick(&[32usize, 40]);
                let v = rng.pick(&[0u64, 1, 33, 34, 60, 61, total, u64::MAX]);
                data[array_off + idx * es + field..array_off + idx * es + field + 8].copy_from_slice(&v.to_le_bytes());
                let crc = crc32(&data[array_off..array_off + array_len]);
                data[hdr + 88..hdr + 92].copy_from_slice(&crc.to_le_bytes());
                fix_header_crc(&mut data[hdr..hdr + SS as usize]);
                desc = format!("entry{}_field@{field}={v}", idx + 1);
            }
        }
        let src = open_src("gpt", &data);
        let ctx = format!("seed={seed} round={round} kind={kind} {desc} bytes={}", data.len());

        // 全盘读取：主备回退与判型都不得 panic
        match load_gpt(&src) {
            Ok(Some(g)) => {
                let file_last = (data.len() as u64) / SS - 1;
                assert!(g.ss == 512 || g.ss == 4096, "{ctx}: accepted a table under a non-candidate sector size");
                assert!(g.header.last_usable_lba <= file_last, "{ctx}: accepted last_usable_lba beyond the container");
                check_copy(ParsedCopy::Usable(Box::new(g)), &ctx);
            }
            Ok(None) => {}
            Err(_) => {}
        }

        // 单副本视角：被这一份接受的表必须自洽（否则"另一份掩盖了它"就无人发现）
        let pmbr = pmbr_size_state(&src, SS).unwrap();
        let copy = parse_primary(&src, SS, pmbr);
        check_copy(copy, &ctx);

        let path = src.path.clone();
        drop(src);
        let _ = std::fs::remove_file(&path);
    }
}

/// 两处定向用例（不是随机碰运气）：已知会被"另一份副本"掩盖的损伤必须在这一份上被拒绝，
/// 而双份同时损伤必须整体报错——不得静默成"无表"
#[test]
fn gpt_copy_damage_is_rejected_not_silently_maskable() {
    let base = gpt_fixture();
    let hdr = SS as usize;
    let total = (base.len() as u64) / SS;
    let backup = ((total - 1) * SS) as usize;

    // 主头 header_size = 91（自洽 CRC）：这一份必须判损伤
    let mut data = base.clone();
    data[hdr + 12..hdr + 16].copy_from_slice(&91u32.to_le_bytes());
    fix_header_crc(&mut data[hdr..hdr + SS as usize]);
    let src = open_src("gpt_copy_size", &data);
    assert!(
        matches!(parse_primary(&src, SS, pmbr_size_state(&src, SS).unwrap()), ParsedCopy::CopyDamaged(_)),
        "a header_size below the 92-byte minimum must be a copy-level damage"
    );

    // 主头 CRC 与备份头 CRC 同时改坏：整体必须报错（两份都不可用）
    let mut data = base.clone();
    data[hdr + 16..hdr + 20].fill(0x5A);
    data[backup + 16..backup + 20].fill(0x5A);
    let src = open_src("gpt_copy_crc", &data);
    let r = load_gpt(&src);
    assert!(r.is_err(), "both copies with a bad CRC must not yield a table");

    // LBA1 签名被抹掉 = 此处确实没有主副本：必须由完好的备份副本回退，
    // 而不是把"主副本没有签名"当作"整盘没有表"
    let mut data = base.clone();
    data[hdr..hdr + 8].fill(0);
    let src = open_src("gpt_copy_absent", &data);
    assert!(matches!(load_gpt(&src), Ok(Some(_))), "the intact backup copy must still be usable");

    // 主副本 last_usable_lba 越过盘尾（自洽 CRC）：这一份必须拒绝
    let mut data = base.clone();
    data[hdr + 48..hdr + 56].copy_from_slice(&(total + 1).to_le_bytes());
    fix_header_crc(&mut data[hdr..hdr + SS as usize]);
    let src = open_src("gpt_copy_beyond", &data);
    assert!(
        matches!(parse_primary(&src, SS, pmbr_size_state(&src, SS).unwrap()), ParsedCopy::CopyDamaged(_)),
        "last_usable_lba beyond the container must be a copy-level damage"
    );
}
