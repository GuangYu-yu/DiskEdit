use super::*;
use super::support::*;
use gptman::GPT;
use std::io::Cursor;

#[test]
pub(crate) fn create_gpt_min_capacity_boundaries() {
    // 最小容量 = 2·span+4（512B→68，4Kn→12）；first==last usable 是合法紧边界
    let mut ok = src_from("cg512ok", vec![0u8; 68 * 512]);
    create_gpt(&mut ok, 512, Some([0x11; 16])).unwrap();
    let g = load_gpt(&ok).unwrap().unwrap();
    assert_eq!(g.header.first_usable_lba, 34);
    assert_eq!(g.header.last_usable_lba, 34);
    // gptman 规范侧读回：验证主/备数组在紧边界下不重叠
    let mut cur = Cursor::new(std::fs::read(&ok.path).unwrap());
    GPT::read_from(&mut cur, 512).unwrap();

    let mut bad = src_from("cg512bad", vec![0u8; 67 * 512]);
    assert!(create_gpt(&mut bad, 512, Some([0x11; 16])).is_err());

    let mut ok4 = src_from("cg4knok", vec![0u8; 12 * 4096]);
    create_gpt(&mut ok4, 4096, Some([0x11; 16])).unwrap();
    let g4 = load_gpt(&ok4).unwrap().unwrap();
    assert_eq!(g4.header.first_usable_lba, 6);
    assert_eq!(g4.header.last_usable_lba, 6);

    let mut bad4 = src_from("cg4knbad", vec![0u8; 11 * 4096]);
    assert!(create_gpt(&mut bad4, 4096, Some([0x11; 16])).is_err());
}

/// 在 GPT 镜像上建 msdos 表：LBA1 的旧主头签名必须被清掉（否则 `table_label` 判
/// `GptDamaged`，后续写命令全部拒绝），而同一扇区的其余字节原样保留
#[test]
pub(crate) fn create_mbr_clears_a_stale_gpt_primary_signature() {
    let mut src = src_from_gpt("mbr_over_gpt", 512);
    let mut before = vec![0u8; 512];
    src.read_at(512, &mut before).unwrap();
    create_mbr(&mut src).unwrap();
    let mut after = vec![0u8; 512];
    src.read_at(512, &mut after).unwrap();
    assert_eq!(&after[..8], &[0u8; 8], "GPT primary signature must be cleared");
    assert_eq!(&after[8..], &before[8..], "the rest of LBA1 must be preserved");
    assert_eq!(table_label(&src).unwrap(), TableLabel::Mbr);
    // 残留的头/数组字节不再构成一张可读的 GPT 表
    assert!(load_gpt(&src).unwrap().is_none());
}

#[test]
pub(crate) fn type_guid_constants_are_disk_byte_order() {
    // 内核 include/linux/efi.h EFI_GUID 宏展开的落盘字节（前 3 字段小端）
    assert_eq!(ESP_TYPE_GUID, [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
    assert_eq!(LINUX_FS_TYPE_GUID, [0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4]);
}