#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
use super::*;
use super::support::*;

/// 新 PARTUUID 的版本/variant 位落在磁盘字节序的 raw[7]/raw[8]
#[test]
pub(crate) fn new_partuuid_sets_v4_version_and_rfc4122_variant() {
    let g = new_partuuid().unwrap();
    assert_eq!(g[7] & 0xF0, 0x40, "version nibble must be 4");
    assert_eq!(g[8] & 0xC0, 0x80, "variant bits must be 10");
}

const SS: u64 = 512;

/// 每扇区内容由扇区号与字节位共同决定：任意扇区错位或只复制首扇区都会与期望不等
fn payload(first_lba: u64, sectors: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity((sectors * SS) as usize);
    for lba in first_lba..first_lba + sectors {
        let mut s = [0u8; SS as usize];
        for (j, b) in s.iter_mut().enumerate() {
            *b = (lba as u8).wrapping_mul(37).wrapping_add(j as u8);
        }
        v.extend_from_slice(&s);
    }
    v
}

fn read_range(src: &WriteSource, start_lba: u64, sectors: u64) -> Vec<u8> {
    let mut v = vec![0u8; (sectors * SS) as usize];
    src.read_at(start_lba * SS, &mut v).unwrap();
    v
}

/// 复制成功：目标区间逐字节等于源；源保持原样；新条目类型/长度沿用源
fn copy_case(source_sectors: u64, chunk_sectors: u64) {
    let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 2048 + source_sectors - 1)]);
    let want = payload(2048, source_sectors);
    src.write_at(2048 * SS, &want).unwrap();
    let (g, repair) = gpt_policy::resolve_geometry(&src).unwrap().unwrap();
    let p = prepare_copy(&g, repair, 1, 8192, false).unwrap();
    let num = execute_copy(&mut src, &g, &p, "copy", chunk_sectors * SS, &mut |_| {})
        .unwrap_or_else(|e| panic!("chunk={chunk_sectors}: copy must succeed: {e}"));
    assert_eq!(num, 2);
    assert_eq!(read_range(&src, 8192, source_sectors), want, "chunk={chunk_sectors}: target must equal the source byte-for-byte");
    assert_eq!(read_range(&src, 2048, source_sectors), want, "chunk={chunk_sectors}: copy must leave the source untouched");
    let g2 = table::load_gpt(&src).unwrap().unwrap();
    assert_eq!(
        (g2.entries[1].starting_lba, g2.entries[1].ending_lba),
        (8192, 8192 + source_sectors - 1),
        "chunk={chunk_sectors}: the new entry must span the copied range"
    );
    assert_eq!(g2.entries[1].partition_type_guid, [0x11; 16], "the copy keeps the source type GUID");
    drop(src);
    let _ = std::fs::remove_file(&path);
}

#[test]
pub(crate) fn copy_single_chunk_copies_every_byte() {
    copy_case(512, 512);
}

#[test]
pub(crate) fn copy_multiple_chunks_copies_every_byte() {
    copy_case(512, 128);
}

#[test]
pub(crate) fn copy_tail_remainder_copies_every_byte() {
    // 5 × 100 扇区 + 12 扇区残段
    copy_case(512, 100);
}

/// prepare 与首写之间第三方改表：复制须以携带的几何复核拒绝，目标区一字未写。
/// 源区首扇区填非零——若拒绝失效，复制会把这些字节带进目标区
#[test]
pub(crate) fn execute_copy_refuses_when_table_changed_after_prepare() {
    let ss = 512u64;
    let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
    src.write_at(2048 * ss, &[0xA5u8; 512]).unwrap();
    let (g, repair) = gpt_policy::resolve_geometry(&src).unwrap().unwrap();
    let p = prepare_copy(&g, repair, 1, 8192, false).unwrap();
    tamper_disk_guid(&mut src);
    let before = std::fs::read(&path).unwrap();
    let r = execute_copy(&mut src, &g, &p, "copy", 1024 * 1024, &mut |_| {});
    assert!(r.is_err(), "execute must refuse a table changed after prepare");
    let err = r.unwrap_err().to_string();
    assert!(err.contains("changed outside this tool"), "unexpected error: {err}");
    let mut b = vec![0u8; 512];
    src.read_at(8192 * ss, &mut b).unwrap();
    assert!(b.iter().all(|&x| x == 0), "target must stay untouched");
    // 目标首扇区只是整段里的一格：拒绝承诺的是整镜像一字未写（含表与源区）
    assert_eq!(std::fs::read(&path).unwrap(), before, "a refusal must leave the image byte-identical");
    drop(src);
    let _ = std::fs::remove_file(&path);
}