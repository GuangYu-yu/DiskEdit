//! 盘上定长布局：GPT 头、GPT 分区条目与 MBR 的 LBA0。
//!
//! 结构只声明字段顺序与宽度，字节读写仍按字段偏移逐字段做 little-endian 转换——
//! 盘上整数的字节序与宿主无关，不能按本机端序整块映射。
//! 每个 `offset_of!` 断言都在编译期核对声明与规范偏移：字段顺序或宽度改错即编译失败，
//! 读写的偏移量因此只有一个来源。

use std::mem::offset_of;

// ---------- GPT 头（UEFI 2.10 §5.3.2 Table 5.5） ----------

/// 头部固定字段：92 字节，其后至 HeaderSize 为保留区
#[repr(C)]
pub(crate) struct GptHeaderLayout {
    pub signature: [u8; 8],
    pub revision: u32,
    pub header_size: u32,
    pub header_crc32: u32,
    pub reserved: u32,
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub disk_guid: [u8; 16],
    pub partition_entry_lba: u64,
    pub number_of_partition_entries: u32,
    pub size_of_partition_entry: u32,
    pub partition_entry_array_crc32: u32,
}

/// 头固定字段的字节数（规范字段表结束处），也是 HeaderSize 的下界
pub(crate) const GPT_HEADER_FIXED_BYTES: usize = 92;
/// 头内 CRC32 字段的宽度
pub(crate) const GPT_CRC_BYTES: usize = 4;

const _: () = {
    assert!(offset_of!(GptHeaderLayout, signature) == 0);
    assert!(offset_of!(GptHeaderLayout, revision) == 8);
    assert!(offset_of!(GptHeaderLayout, header_size) == 12);
    assert!(offset_of!(GptHeaderLayout, header_crc32) == 16);
    assert!(offset_of!(GptHeaderLayout, reserved) == 20);
    assert!(offset_of!(GptHeaderLayout, my_lba) == 24);
    assert!(offset_of!(GptHeaderLayout, alternate_lba) == 32);
    assert!(offset_of!(GptHeaderLayout, first_usable_lba) == 40);
    assert!(offset_of!(GptHeaderLayout, last_usable_lba) == 48);
    assert!(offset_of!(GptHeaderLayout, disk_guid) == 56);
    assert!(offset_of!(GptHeaderLayout, partition_entry_lba) == 72);
    assert!(offset_of!(GptHeaderLayout, number_of_partition_entries) == 80);
    assert!(offset_of!(GptHeaderLayout, size_of_partition_entry) == 84);
    assert!(offset_of!(GptHeaderLayout, partition_entry_array_crc32) == 88);
    assert!(offset_of!(GptHeaderLayout, partition_entry_array_crc32) + GPT_CRC_BYTES == GPT_HEADER_FIXED_BYTES);
};

// ---------- GPT 分区条目（UEFI 2.10 §5.3.3 Table 5.6） ----------

/// 分区名的码元数：36 个 UTF-16 码元 = 72 字节
pub(crate) const GPT_NAME_UNITS: usize = 36;
pub(crate) const GPT_NAME_BYTES: usize = GPT_NAME_UNITS * 2;

#[repr(C)]
pub(crate) struct GptEntryLayout {
    pub partition_type_guid: [u8; 16],
    pub unique_partition_guid: [u8; 16],
    pub starting_lba: u64,
    pub ending_lba: u64,
    pub attributes: u64,
    pub name: [u8; GPT_NAME_BYTES],
}

/// 单条目规范字段的字节数：SizeOfPartitionEntry 的取值下界，其后为保留区
pub(crate) const GPT_ENTRY_FIXED_BYTES: usize = 128;
/// 分区名字段在条目内的起点
pub(crate) const GPT_ENTRY_NAME_OFFSET: usize = offset_of!(GptEntryLayout, name);

const _: () = {
    assert!(offset_of!(GptEntryLayout, partition_type_guid) == 0);
    assert!(offset_of!(GptEntryLayout, unique_partition_guid) == 16);
    assert!(offset_of!(GptEntryLayout, starting_lba) == 32);
    assert!(offset_of!(GptEntryLayout, ending_lba) == 40);
    assert!(offset_of!(GptEntryLayout, attributes) == 48);
    assert!(offset_of!(GptEntryLayout, name) == 56);
    assert!(offset_of!(GptEntryLayout, name) + GPT_NAME_BYTES == GPT_ENTRY_FIXED_BYTES);
};

// ---------- MBR 的 LBA0（UEFI 2.10 §5.2.1 Table 5.1） ----------

#[repr(C)]
pub(crate) struct MbrEntryLayout {
    pub boot_indicator: u8,
    pub start_chs: [u8; 3],
    pub os_type: u8,
    pub end_chs: [u8; 3],
    pub starting_lba: u32,
    pub size_in_lba: u32,
}

/// LBA0 整体：引导代码、四条分区记录、签名紧邻，段间没有填充字节，
/// 故按 `packed` 声明（结构只用于核对偏移，不做整块映射）
#[repr(C, packed)]
pub(crate) struct MbrLayout {
    pub boot_code: [u8; MBR_ENTRY_BASE],
    pub entries: [MbrEntryLayout; MBR_ENTRY_SLOTS],
    pub signature: u16,
}

/// 引导代码区长度，也是分区表区起点
pub(crate) const MBR_ENTRY_BASE: usize = 446;
/// 分区表槽位数
pub(crate) const MBR_ENTRY_SLOTS: usize = 4;
/// 单条分区记录的字节数
pub(crate) const MBR_ENTRY_BYTES: usize = 16;
/// 签名在 LBA0 内的起点
pub(crate) const MBR_SIGNATURE_AT: usize = 510;
/// LBA0 内被分区表与签名占用的区间
pub(crate) const MBR_SIGNED_BYTES: usize = 512;

const _: () = {
    assert!(offset_of!(MbrEntryLayout, boot_indicator) == 0);
    assert!(offset_of!(MbrEntryLayout, start_chs) == 1);
    assert!(offset_of!(MbrEntryLayout, os_type) == 4);
    assert!(offset_of!(MbrEntryLayout, end_chs) == 5);
    assert!(offset_of!(MbrEntryLayout, starting_lba) == 8);
    assert!(offset_of!(MbrEntryLayout, size_in_lba) == 12);
    assert!(offset_of!(MbrLayout, entries) == MBR_ENTRY_BASE);
    assert!(offset_of!(MbrLayout, signature) == MBR_SIGNATURE_AT);
    assert!(MBR_ENTRY_SLOTS * MBR_ENTRY_BYTES == MBR_SIGNATURE_AT - MBR_ENTRY_BASE);
    assert!(MBR_SIGNATURE_AT + 2 == MBR_SIGNED_BYTES);
};

/// 第 `slot` 条 MBR 分区记录在 LBA0 内的字节区间（`slot` ∈ `0..MBR_ENTRY_SLOTS`）
pub(crate) fn mbr_entry_range(slot: usize) -> std::ops::Range<usize> {
    let start = MBR_ENTRY_BASE + slot * MBR_ENTRY_BYTES;
    start..start + MBR_ENTRY_BYTES
}

// ---------- 字节读写 ----------

/// 把 little-endian u32 写进 `off` 起的 4 字节
pub(crate) fn wr_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// 把 little-endian u64 写进 `off` 起的 8 字节
pub(crate) fn wr_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
