use super::*;
use super::support::*;
use gptman::GPT;
use std::io::Cursor;

#[test]
pub(crate) fn create_gpt_min_capacity_boundaries() {
    // 最小容量 = 2·span+4（512B→68，4Kn→12）；first==last usable 是合法紧边界
    let mut ok = src_from("cg512ok", vec![0u8; 68 * 512]);
    create_gpt(&mut ok, 512, Some([0x11; 16]), false).unwrap();
    let g = load_gpt(&ok).unwrap().unwrap();
    assert_eq!(g.header.first_usable_lba, 34);
    assert_eq!(g.header.last_usable_lba, 34);
    // gptman 规范侧读回：验证主/备数组在紧边界下不重叠
    let mut cur = Cursor::new(std::fs::read(&ok.path).unwrap());
    GPT::read_from(&mut cur, 512).unwrap();

    let mut bad = src_from("cg512bad", vec![0u8; 67 * 512]);
    assert!(create_gpt(&mut bad, 512, Some([0x11; 16]), false).is_err());

    let mut ok4 = src_from("cg4knok", vec![0u8; 12 * 4096]);
    create_gpt(&mut ok4, 4096, Some([0x11; 16]), false).unwrap();
    let g4 = load_gpt(&ok4).unwrap().unwrap();
    assert_eq!(g4.header.first_usable_lba, 6);
    assert_eq!(g4.header.last_usable_lba, 6);

    let mut bad4 = src_from("cg4knbad", vec![0u8; 11 * 4096]);
    assert!(create_gpt(&mut bad4, 4096, Some([0x11; 16]), false).is_err());
}

/// 在 GPT 镜像上建 msdos 表：LBA1 的旧主头签名必须被清掉（否则 `table_label` 判
/// `GptDamaged`，后续写命令全部拒绝），而同一扇区的其余字节原样保留
#[test]
pub(crate) fn create_mbr_clears_a_stale_gpt_primary_signature() {
    let mut src = src_from_gpt("mbr_over_gpt", 512);
    let mut before = vec![0u8; 512];
    src.read_at(512, &mut before).unwrap();
    create_mbr(&mut src, false).unwrap();
    let mut after = vec![0u8; 512];
    src.read_at(512, &mut after).unwrap();
    assert_eq!(&after[..8], &[0u8; 8], "GPT primary signature must be cleared");
    assert_eq!(&after[8..], &before[8..], "the rest of LBA1 must be preserved");
    assert_eq!(table_label(&src).unwrap(), TableLabel::Mbr);
    // 残留的头/数组字节不再构成一张可读的 GPT 表
    assert!(load_gpt(&src).unwrap().is_none());
}

/// 容器扇区大于 512 时，候选签名位置 512 落在 LBA0 的写回缓冲里：清零必须先施加到
/// 最终缓冲，否则整块回写会把刚清掉的签名重新覆盖成 `EFI PART`
#[test]
pub(crate) fn create_mbr_clears_signature_inside_lba0_on_4kn_container() {
    let ss = 4096u64;
    let mut data = vec![0u8; 100 * ss as usize];
    data[512..520].copy_from_slice(GPT_SIGNATURE);
    let mut src = src_from_ss("mbr_4kn_sig512", data, ss);
    // 512 处的签名不是容器 ss 下的 LBA1：旧表读不出来，写入口按未授权覆盖拒绝，
    // 显式授权才走"清零签名"这条路径（本用例测的正是清零落点）
    create_mbr(&mut src, true).unwrap();
    let mut got = vec![0u8; GPT_SIGNATURE.len()];
    src.read_at(512, &mut got).unwrap();
    assert_eq!(got, vec![0u8; GPT_SIGNATURE.len()], "signature at byte 512 must end up zero");
}

#[test]
pub(crate) fn type_guid_constants_are_disk_byte_order() {
    // 内核 include/linux/efi.h EFI_GUID 宏展开的落盘字节（前 3 字段小端）
    assert_eq!(ESP_TYPE_GUID, [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
    assert_eq!(LINUX_FS_TYPE_GUID, [0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4]);
    // util-linux libfdisk/src/pt-gpt-partnames.h：Linux LUKS / Linux LVM
    assert_eq!(LUKS_TYPE_GUID, [0xCB, 0x7C, 0x7D, 0xCA, 0xED, 0x63, 0x53, 0x4C, 0x86, 0x1C, 0x17, 0x42, 0x53, 0x60, 0x59, 0xCC]);
    assert_eq!(LVM_TYPE_GUID, [0x79, 0xD3, 0xD6, 0xE6, 0x07, 0xF5, 0xC2, 0x44, 0xA2, 0x3C, 0x23, 0x8F, 0x2A, 0x3D, 0xF9, 0x28]);
}

/// 字段序摆法是同一常量的派生，不是另一份字面量：三字段倒序即互转（对合），后 8 字节
/// 在两种摆法里相同，写路径的常量保持磁盘序
#[test]
pub(crate) fn guid_field_order_is_a_self_inverse_permutation() {
    for g in [ESP_TYPE_GUID, LINUX_FS_TYPE_GUID, SWAP_TYPE_GUID, LUKS_TYPE_GUID, LVM_TYPE_GUID] {
        let natural = guid_in_field_order(&g);
        assert_eq!(guid_in_field_order(&natural), g, "两种摆法互转须回到原值");
        assert_eq!(natural[8..], g[8..], "后 8 字节不参与端序转换");
    }
    // 标准文本 0657FD6D-A4AB-43C4-84E5-0933C84B4F4F 的字段序摆放
    assert_eq!(
        guid_in_field_order(&SWAP_TYPE_GUID),
        [0x06, 0x57, 0xFD, 0x6D, 0xA4, 0xAB, 0x43, 0xC4, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F]
    );
}

/// 旧表形态：4 条目 ⇒ 跨度 1 扇区，可用区从 LBA3 起。新表按 128 条目算出的主数组
/// 覆盖 [LBA2, LBA34)，于是从这个位置起的旧分区会落到新表的表结构区
fn write_small_span_gpt(src: &mut WriteSource, ss: u64) {
    let geom = EntryArrayGeometry::new(ss, 128, 4, MAX_ARRAY_BYTES).unwrap();
    let entries = vec![GPTPartitionEntry {
        partition_type_guid: [0x11; 16],
        unique_partition_guid: [0x22; 16],
        starting_lba: 3,
        ending_lba: 9,
        attribute_bits: 0,
        partition_name: "".into(),
    }];
    let (bytes, crc) = serialize_array(&entries, &geom);
    src.write_at(2 * ss, &bytes).unwrap();
    let last_lba = src.size / ss - 1;
    let h = RawHeader {
        revision: 0x0001_0000,
        primary_lba: 1,
        backup_lba: last_lba,
        first_usable_lba: 3,
        last_usable_lba: last_lba - geom.lba_span() - 1,
        disk_guid: [0; 16],
        partition_entry_lba: 2,
        number_of_partition_entries: 4,
        size_of_partition_entry: 128,
        header_size: 92,
    };
    let sec = serialize_header(&h, crc, ss).unwrap();
    src.write_at(ss, &sec).unwrap();
}

/// GPT 冲突检查不得依赖保护 MBR 形状：LBA0 被外部工具改坏而 LBA1 仍是完整 GPT 时，
/// `load_gpt` 返回 Ok(None)、`parse_mbr_raw` 也因主头可解析返回 None——若冲突检查复用
/// 这两条路径就会静默通过，`new --table gpt` 于是把旧分区数据清零而不要求授权
#[test]
pub(crate) fn gpt_conflict_check_ignores_pmbr_shape() {
    let mut src = src_from("cfchk_pmbr", vec![0u8; 100 * 512]);
    write_small_span_gpt(&mut src, 512);
    // LBA0 全零：保护 MBR 形状不成立，盘型判定的两路都看不见这张表
    assert!(!super::super::mbr::pmbr_shape_valid(&src).unwrap());
    assert!(load_gpt(&src).unwrap().is_none(), "the shape gate must hide the table from the normal parser");

    let why = gpt_metadata_conflict(&src, 512).unwrap().expect("the conflict must be found");
    assert!(why.contains("GPT partition 1"), "{why}");
    assert!(why.contains("primary entry array"), "{why}");
}

/// 旧表整体不可证实（LBA1 有签名但头 CRC 不符、盘尾也没有可回退的副本）时，冲突检查
/// 必须报错而不是报"无冲突"：这种现场下无法证明新表写入只落在表结构区
#[test]
pub(crate) fn gpt_conflict_check_fails_closed_on_unreadable_old_table() {
    let mut src = src_from("cfchk_bad", vec![0u8; 100 * 512]);
    write_small_span_gpt(&mut src, 512);
    // 签名仍在、头 CRC 被改坏：主副本不可用，盘尾没有备份副本
    let mut sec = vec![0u8; 512];
    src.read_at(512, &mut sec).unwrap();
    sec[16..20].fill(0);
    src.write_at(512, &sec).unwrap();

    let e = gpt_metadata_conflict(&src, 512).expect_err("an unverifiable old table must not read as 'no conflict'");
    assert!(e.to_string().contains("header"), "{e}");
}

/// MBR 建表要清理的 GPT 签名位置若落在旧表分区数据里，必须被冲突检查拦下：
/// 签名匹配只说明"这里要清"，不构成覆盖分区字节的授权
#[test]
pub(crate) fn mbr_conflict_catches_gpt_signature_inside_partition() {
    let ss = 512u64;
    // 分区 [1..=99] 覆盖整盘的可用区，盘尾那份 GPT 备份头就落在分区数据里
    let mut src = src_from_mbr("cfchk_mbr", &[(0x83, 1, 99)], 100);
    let geom = EntryArrayGeometry::new(ss, 128, 4, MAX_ARRAY_BYTES).unwrap();
    let (bytes, crc) = serialize_array(&[], &geom);
    src.write_at(98 * ss, &bytes).unwrap();
    let h = RawHeader {
        revision: 0x0001_0000,
        primary_lba: 99,
        backup_lba: 1,
        first_usable_lba: 3,
        last_usable_lba: 97,
        disk_guid: [0; 16],
        partition_entry_lba: 98,
        number_of_partition_entries: 4,
        size_of_partition_entry: 128,
        header_size: 92,
    };
    let sec = serialize_header(&h, crc, ss).unwrap();
    src.write_at(99 * ss, &sec).unwrap();

    let why = mbr_metadata_conflict(&src, ss).unwrap().expect("the conflict must be found");
    assert!(why.contains("MBR partition 1"), "{why}");
    assert!(why.contains("GPT header bytes"), "{why}");

    // 没有可清的签名就不报冲突：普通 msdos 建表不被这条检查误拦
    let plain = src_from_mbr("cfchk_mbr_plain", &[(0x83, 1, 99)], 100);
    assert!(mbr_metadata_conflict(&plain, ss).unwrap().is_none());
}

/// 命令层判定通过之后、首次写盘之前盘上的表已变：GPT 写入口按**当前**盘面重判，
/// 拒绝且一个字节都不写；若只在命令层判一次，这里会照常写下新表、把旧分区数据清零
#[test]
pub(crate) fn create_gpt_refuses_a_table_that_changed_after_the_command_check() {
    let mut src = src_from("gate_gpt", vec![0u8; 100 * 512]);
    assert!(new_table_conflict(&src, 512, TableKind::Gpt).unwrap().is_none(), "the command-layer check sees a clean disk");

    // 首次写盘之前表已变：旧表把分区铺进新表的主条目数组区 [LBA2, LBA34)
    write_small_span_gpt(&mut src, 512);
    let before = std::fs::read(&src.path).unwrap();

    let e = create_gpt(&mut src, 512, Some([0x11; 16]), false)
        .expect_err("a conflict that appeared after the command-layer check must still be refused");
    assert!(matches!(&e, crate::outcome::RunOutcome::Refused(_)), "{e}");
    assert_eq!(e.exit_code(), crate::outcome::EXIT_REFUSED);
    assert_eq!(std::fs::read(&src.path).unwrap(), before, "the refusal must not write a single byte");
}

/// 同一条窗口的 msdos 侧：`create_mbr` 要清零的盘尾 GPT 签名落进了新出现的 MBR 分区，
/// 写入口拒绝且不落盘；若只在命令层判一次，这里会先清签名、再写 LBA0
#[test]
pub(crate) fn create_mbr_refuses_when_a_cleared_signature_lands_inside_a_partition() {
    let ss = 512u64;
    let mut src = src_from("gate_mbr", vec![0u8; 100 * ss as usize]);
    assert!(new_table_conflict(&src, ss, TableKind::Mbr).unwrap().is_none(), "the command-layer check sees a clean disk");

    // 首次写盘之前表已变：MBR 分区从 LBA1 铺到盘尾，盘尾带一份自洽的 GPT 备份头
    let mut lba0 = vec![0u8; ss as usize];
    lba0[446 + 4] = 0x83;
    lba0[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    lba0[446 + 12..446 + 16].copy_from_slice(&99u32.to_le_bytes());
    lba0[510] = 0x55;
    lba0[511] = 0xAA;
    src.write_at(0, &lba0).unwrap();
    let geom = EntryArrayGeometry::new(ss, 128, 4, MAX_ARRAY_BYTES).unwrap();
    let (bytes, crc) = serialize_array(&[], &geom);
    src.write_at(98 * ss, &bytes).unwrap();
    let h = RawHeader {
        revision: 0x0001_0000,
        primary_lba: 99,
        backup_lba: 1,
        first_usable_lba: 3,
        last_usable_lba: 97,
        disk_guid: [0; 16],
        partition_entry_lba: 98,
        number_of_partition_entries: 4,
        size_of_partition_entry: 128,
        header_size: 92,
    };
    let sec = serialize_header(&h, crc, ss).unwrap();
    src.write_at(99 * ss, &sec).unwrap();
    let before = std::fs::read(&src.path).unwrap();

    let e = create_mbr(&mut src, false)
        .expect_err("a conflict that appeared after the command-layer check must still be refused");
    assert!(matches!(&e, crate::outcome::RunOutcome::Refused(_)), "{e}");
    assert_eq!(e.exit_code(), crate::outcome::EXIT_REFUSED);
    assert_eq!(std::fs::read(&src.path).unwrap(), before, "the refusal must not write a single byte");
}