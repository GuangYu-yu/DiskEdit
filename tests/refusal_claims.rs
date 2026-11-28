//! 拒绝点的簇级覆盖：按拒绝类别成簇，用库的公开入口复现同一拒绝。
//!
//! 每条用例断言具体错误形态，不依赖实现细节；命令行入口由 `tests/case_runner.rs` 覆盖。

use diskedit_core::table::edit::{
    add_mbr, delete_mbr, mbr_last_live, resize_mbr, set_mbr_hidden, set_mbr_type,
};
use diskedit_core::table::mbr::primary::parse;
use diskedit_core::table::{
    ENTRY_MIN_BYTES, EditError, GptFlag, GptHeader, GptTable, Guid, MAX_ENTRIES, PartitionEntry,
    PartitionName, TableError, first_fit, fixed_start,
};

const SIGNATURE: usize = 510;
const ENTRY_BASE: usize = 446;
const ENTRY_BYTES: usize = 16;

/// 构造一扇区 MBR；`entries` 是 (boot, type, start, size)。
fn mbr_sector(entries: &[(u8, u8, u32, u32)]) -> [u8; 512] {
    let mut sector = [0u8; 512];
    for (index, &(boot, kind, start, size)) in entries.iter().enumerate() {
        let offset = ENTRY_BASE + index * ENTRY_BYTES;
        sector[offset] = boot;
        sector[offset + 4] = kind;
        sector[offset + 8..offset + 12].copy_from_slice(&start.to_le_bytes());
        sector[offset + 12..offset + 16].copy_from_slice(&size.to_le_bytes());
    }
    sector[SIGNATURE] = 0x55;
    sector[SIGNATURE + 1] = 0xAA;
    sector
}

fn gpt_header() -> GptHeader {
    GptHeader {
        revision: 0x0001_0000,
        primary_lba: 1,
        backup_lba: 1000,
        first_usable_lba: 34,
        last_usable_lba: 967,
        disk_guid: Guid::ZERO,
        partition_entry_lba: 2,
        number_of_partition_entries: MAX_ENTRIES,
        size_of_partition_entry: ENTRY_MIN_BYTES as u32,
        header_size: 92,
        entry_array_crc32: 0,
    }
}

fn entry(number: u8, start: u64, end: u64, defined: bool) -> PartitionEntry {
    let mut type_bytes = [0u8; 16];
    if defined {
        type_bytes[0] = 0x0c;
    }
    let mut unique = [0u8; 16];
    unique[0] = number;
    PartitionEntry {
        partition_type_guid: Guid(type_bytes),
        unique_partition_guid: Guid(unique),
        starting_lba: start,
        ending_lba: end,
        attributes: 0,
        name: PartitionName::EMPTY,
    }
}

fn gpt_table(entries: Vec<PartitionEntry>) -> GptTable {
    GptTable {
        header: gpt_header(),
        entries,
        sector_size: 512,
        total_bytes: 1001 * 512,
    }
}

#[test]
fn mbr_parse_rejects_unsigned_and_short_sectors() {
    let unsigned = [0u8; 512];
    assert!(matches!(
        parse(&unsigned),
        Err(TableError::MbrSignatureMissing)
    ));

    let short = vec![0u8; 20];
    assert!(parse(&short).is_err(), "短缓冲区必须被拒绝");
}

#[test]
fn mbr_add_rejects_types_ranges_and_overlaps() {
    let sector = mbr_sector(&[(0, 0x83, 2048, 1024)]);
    let total = 8192;

    for kind in [0x00, 0xEE, 0x05] {
        let error = add_mbr(&sector, None, 4096, 5000, kind, total).expect_err("类型应被拒绝");
        assert!(
            matches!(
                error,
                EditError::ZeroTypeByte
                    | EditError::ProtectiveTypeByte
                    | EditError::ContainerTypeByte(_)
            ),
            "实际错误：{error}"
        );
    }

    assert!(matches!(
        add_mbr(&sector, None, 4096, 5000, 0x83, 1),
        Err(EditError::OutOfRange { .. })
    ));
    assert!(matches!(
        add_mbr(&sector, None, 0, 5000, 0x83, total),
        Err(EditError::OutOfRange { .. })
    ));
    assert!(matches!(
        add_mbr(&sector, None, 4096, total, 0x83, total),
        Err(EditError::OutOfRange { .. })
    ));
    assert!(matches!(
        add_mbr(
            &sector,
            None,
            4096,
            u64::from(u32::MAX) + 5,
            0x83,
            u64::from(u32::MAX) + 10
        ),
        Err(EditError::PastAddressSpace { .. })
    ));
    assert!(matches!(
        add_mbr(&sector, None, 2048, 3000, 0x83, total),
        Err(EditError::Overlaps { .. })
    ));
}

#[test]
fn mbr_add_rejects_slot_limits_and_invisible_entries() {
    let occupied = mbr_sector(&[(0, 0x83, 2048, 1024)]);
    let total = 8192;
    assert!(matches!(
        add_mbr(&occupied, Some(5), 4096, 5000, 0x83, total),
        Err(EditError::SlotOutOfRange { max: 4, .. })
    ));
    assert!(matches!(
        add_mbr(&occupied, Some(1), 4096, 5000, 0x83, total),
        Err(EditError::SlotOccupied(1))
    ));

    let invisible = mbr_sector(&[(0, 0x83, 2048, 0)]);
    assert!(matches!(
        add_mbr(&invisible, None, 4096, 5000, 0x83, total),
        Err(EditError::UnusableTable(_))
    ));

    let full = mbr_sector(&[
        (0, 0x83, 100, 10),
        (0, 0x83, 200, 10),
        (0, 0x83, 300, 10),
        (0, 0x83, 400, 10),
    ]);
    assert!(matches!(
        add_mbr(&full, None, 4096, 5000, 0x83, total),
        Err(EditError::TableFull)
    ));
}

#[test]
fn mbr_delete_rejects_empty_structural_and_out_of_range_slots() {
    let empty = mbr_sector(&[]);
    assert!(matches!(
        delete_mbr(&empty, 1),
        Err(EditError::PartitionEmpty(1))
    ));
    assert!(matches!(
        delete_mbr(&empty, 0),
        Err(EditError::SlotOutOfRange { max: 4, .. })
    ));
    assert!(matches!(
        delete_mbr(&empty, 5),
        Err(EditError::SlotOutOfRange { max: 4, .. })
    ));

    let protective = mbr_sector(&[(0, 0xEE, 1, 8191)]);
    assert!(matches!(
        delete_mbr(&protective, 1),
        Err(EditError::ProtectiveTypeByte)
    ));

    let container = mbr_sector(&[(0, 0x05, 2048, 1024)]);
    assert!(matches!(
        delete_mbr(&container, 1),
        Err(EditError::ContainerTypeByte(0x05))
    ));
}

#[test]
fn mbr_resize_rejects_structural_and_out_of_bounds_ranges() {
    let protective = mbr_sector(&[(0, 0xEE, 1, 8191)]);
    assert!(matches!(
        resize_mbr(&protective, 1, 1, 100, 8192),
        Err(EditError::ProtectiveTypeByte)
    ));

    let container = mbr_sector(&[(0, 0x0F, 2048, 1024)]);
    assert!(matches!(
        resize_mbr(&container, 1, 2048, 3072, 8192),
        Err(EditError::ContainerTypeByte(0x0F))
    ));

    let sector = mbr_sector(&[(0, 0x83, 2048, 1024)]);
    assert!(matches!(
        resize_mbr(&sector, 1, 0, 3000, 8192),
        Err(EditError::OutOfRange { .. })
    ));
    assert!(matches!(
        resize_mbr(&sector, 1, 2048, 2047, 8192),
        Err(EditError::OutOfRange { .. })
    ));
    assert!(matches!(
        resize_mbr(&sector, 1, 2048, 8192, 8192),
        Err(EditError::OutOfRange { .. })
    ));
    assert!(matches!(
        resize_mbr(
            &sector,
            1,
            4096,
            u64::from(u32::MAX) + 5,
            u64::from(u32::MAX) + 10
        ),
        Err(EditError::PastAddressSpace { .. })
    ));
}

#[test]
fn mbr_type_and_hidden_reject_structural_types() {
    let sector = mbr_sector(&[(0, 0x83, 2048, 1024)]);
    assert!(matches!(
        set_mbr_type(&sector, 1, 0x00),
        Err(EditError::ZeroTypeByte)
    ));
    assert!(matches!(
        set_mbr_type(&sector, 1, 0xEE),
        Err(EditError::ProtectiveTypeByte)
    ));
    assert!(matches!(
        set_mbr_type(&sector, 1, 0x05),
        Err(EditError::ContainerTypeByte(0x05))
    ));

    let protective = mbr_sector(&[(0, 0xEE, 1, 8191)]);
    assert!(matches!(
        set_mbr_type(&protective, 1, 0x83),
        Err(EditError::ProtectiveTypeByte)
    ));

    let container = mbr_sector(&[(0, 0x85, 2048, 1024)]);
    assert!(matches!(
        set_mbr_type(&container, 1, 0x83),
        Err(EditError::ContainerTypeByte(0x85))
    ));

    let exotic = mbr_sector(&[(0, 0x83, 2048, 1024)]);
    assert!(matches!(
        set_mbr_hidden(&exotic, 1, true),
        Err(EditError::NoHiddenCounterpart(0x83))
    ));
}

#[test]
fn partition_lookup_rejects_empty_and_missing_entries() {
    let table = gpt_table(vec![entry(1, 34, 40, false)]);
    assert!(matches!(
        table.live_index(1),
        Err(EditError::PartitionEmpty(1))
    ));
    assert!(matches!(
        table.live_index(9),
        Err(EditError::PartitionMissing(9))
    ));
    assert_eq!(table.last_live(), None);

    let mbr = mbr_sector(&[]);
    assert_eq!(mbr_last_live(&mbr).expect("空 MBR 应可解析"), None);
}

#[test]
fn gpt_add_slot_rejects_bounds_overlap_slots_and_full_table() {
    let table = gpt_table(vec![
        entry(1, 100, 200, true),
        entry(2, 0, 0, false),
        entry(3, 0, 0, false),
        entry(4, 0, 0, false),
    ]);
    assert!(matches!(
        table.add_slot(None, 10, 20),
        Err(EditError::OutsideUsable { .. })
    ));
    assert!(matches!(
        table.add_slot(None, 150, 250),
        Err(EditError::Overlaps { .. })
    ));
    assert!(matches!(
        table.add_slot(Some(9), 300, 400),
        Err(EditError::SlotOutOfRange { .. })
    ));
    assert!(matches!(
        table.add_slot(Some(1), 300, 400),
        Err(EditError::SlotOccupied(1))
    ));

    let full = gpt_table(vec![
        entry(1, 100, 110, true),
        entry(2, 120, 130, true),
        entry(3, 140, 150, true),
        entry(4, 160, 170, true),
    ]);
    assert!(matches!(
        full.add_slot(None, 300, 400),
        Err(EditError::TableFull)
    ));
}

#[test]
fn gpt_usable_range_rejects_inverted_bounds() {
    let mut header = gpt_header();
    header.first_usable_lba = 968;
    header.last_usable_lba = 967;
    assert!(diskedit_core::policy::gpt::usable_range(&header).is_err());
}

#[test]
fn gpt_flag_names_reject_unknown_values() {
    assert!(GptFlag::parse("hidden").is_some());
    assert!(GptFlag::parse("no-such-flag").is_none());
}

#[test]
fn relocation_layout_rejects_zero_geometry_and_unaddressable_shifts() {
    use diskedit_core::table::relocate::layout;
    assert!(layout(10, 0, 1, 512).is_none(), "零扇区尺寸应无布局");
    let copy = u64::MAX / 2;
    assert!(
        layout(copy, 512, i64::MAX - 1, 1 << 20).is_none(),
        "无法寻址的位移应无布局"
    );
}

#[test]
fn recovery_text_reports_the_possible_change_and_the_cause() {
    let text = diskedit::app::report::text::recovery("the write barrier failed");
    assert!(
        text.contains("the target may already have been changed"),
        "{text}"
    );
    assert!(text.contains("cause: the write barrier failed"), "{text}");
}

#[test]
fn a_record_for_another_target_is_not_resumed() {
    use diskedit_core::fingerprint::Fingerprint;
    use diskedit_core::record::{RecordBody, RecoveryPlan, encode, recover};
    let body = RecordBody {
        target: Fingerprint::strong([0x11; 32]),
        legacy_generation: 0,
        plan_digest: [0u8; 32],
    };
    let bytes = encode(&body, &[]).expect("记录可编码");
    let plan = recover(&bytes, Fingerprint::strong([0x22; 32]));
    assert!(
        matches!(plan, RecoveryPlan::NeedsManual { .. }),
        "别人的记录不得续跑：{plan:?}"
    );
}

#[test]
fn alignment_overflow_rejects_the_start() {
    use diskedit_core::geometry::align;
    assert_eq!(
        align::up(u64::MAX, 4096),
        Err(diskedit_core::geometry::GeometryError::Overflow)
    );
}

#[test]
fn gpt_placement_rejects_address_space_overflow() {
    assert!(matches!(
        fixed_start(&[], 34, u64::MAX, 2048, 10, u64::MAX),
        Err(EditError::PastAddressSpace { .. })
    ));
    assert!(matches!(
        fixed_start(&[], 34, u64::MAX, 1, 1000, u64::MAX - 100),
        Err(EditError::PastAddressSpace { .. })
    ));
    assert!(matches!(
        first_fit(&[], 34, 967, 1, 0),
        Err(EditError::ZeroSize)
    ));
    assert!(matches!(
        first_fit(&[], 34, 967, 1000, 10),
        Err(EditError::NoGapFits { .. })
    ));
}

#[test]
fn an_unusable_identity_cannot_plan_a_write() {
    use diskedit::app::decide::{Plan, Refusal};
    use diskedit::app::report::exit::{UNSUPPORTED, exit_of_refusal};
    use diskedit_core::fingerprint::Fingerprint;

    // 只有 Strong 等级能驱动写计划；dm/md、仅读数不一致等不可用身份落 6。
    let error = Plan::write(Vec::new(), Fingerprint::unusable([0x11; 32]))
        .expect_err("不可用身份不得构造写计划");
    assert!(
        matches!(error, Refusal::IdentityUnprovable { .. }),
        "实际拒绝：{error:?}"
    );
    assert_eq!(exit_of_refusal(&error), UNSUPPORTED);
}

#[test]
fn mbr_resize_rejects_short_empty_invisible_and_overlapping_states() {
    // 快照短于一条 MBR 记录区：不可当作表使用。
    assert!(matches!(
        resize_mbr(&[0u8; 20], 1, 1, 2, 100),
        Err(EditError::UnusableTable(_))
    ));
    // 空槽不能作为扩缩对象。
    let empty = mbr_sector(&[]);
    assert!(matches!(
        resize_mbr(&empty, 1, 2048, 3071, 8192),
        Err(EditError::PartitionEmpty(1))
    ));
    // 有类型字节但零扇区：条目对解析侧不可见，拒绝改写。
    let invisible = mbr_sector(&[(0, 0x83, 2048, 0)]);
    assert!(matches!(
        resize_mbr(&invisible, 1, 2048, 3071, 8192),
        Err(EditError::UnusableTable(_))
    ));
    // 扩到邻接分区所在区间：重叠必须挡在写 LBA0 之前。
    let two = mbr_sector(&[(0, 0x83, 2048, 1024), (0, 0x83, 4096, 1024)]);
    assert!(matches!(
        resize_mbr(&two, 1, 2048, 5000, 8192),
        Err(EditError::Overlaps { .. })
    ));
    // 只在空隙内扩：成功。
    assert!(resize_mbr(&two, 1, 2048, 4000, 8192).is_ok());
}

#[test]
fn table_capacity_refuses_disks_too_small_for_gpt_or_mbr() {
    use diskedit_core::table::{CapacityError, Format, check_capacity};

    // 4 MiB 的盘放得下 GPT 与 MBR；只有 1 个扇区的盘两者都不行。
    assert!(check_capacity(Format::Gpt, 4 << 20, 512).is_ok());
    assert!(check_capacity(Format::Mbr, 4 << 20, 512).is_ok());
    assert!(matches!(
        check_capacity(Format::Gpt, 512, 512),
        Err(CapacityError::Gpt { .. })
    ));
    assert!(matches!(
        check_capacity(Format::Mbr, 1024, 512),
        Err(CapacityError::Mbr { .. })
    ));
    // 零扇区尺寸是几何错误，两种格式都拒绝。
    assert!(matches!(
        check_capacity(Format::Gpt, 4 << 20, 0),
        Err(CapacityError::Mbr { .. })
    ));
}
