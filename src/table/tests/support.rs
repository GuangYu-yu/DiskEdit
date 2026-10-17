use super::*;
use gptman::GPT;
use std::io::Cursor;

pub(crate) fn fixture_gpt(ss: u64) -> Vec<u8> {
    let data = vec![0u8; 100 * ss as usize];
    let mut cur = Cursor::new(data);
    let mut gpt = GPT::new_from(&mut cur, ss, [0xAB; 16]).unwrap();
    gpt[1] = GPTPartitionEntry {
        partition_type_guid: [0x01; 16],
        unique_partition_guid: [0x02; 16],
        starting_lba: 34,
        ending_lba: 60,
        attribute_bits: 0,
        partition_name: "test".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    cur.into_inner()
}

pub(crate) fn src_from(tag: &str, data: Vec<u8>) -> FileSource {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("diskedit_test_{tag}_{}.img", std::process::id()));
    std::fs::write(&tmp, &data).unwrap();
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&tmp).unwrap();
    let size = data.len() as u64;
    FileSource {
        identity: crate::dev::TargetIdentity::resolve_image(&tmp),
        file: f,
        path: tmp,
        sector_size: 512,
        size,
        is_block: false,
        journal: None,
        ownership: None,
        fingerprint: Default::default(),
        loop_mapping: None,
    }
}

/// GPT 测试镜像：gptman 只写 GPT 结构，保护 MBR 需自行补——load_gpt 以前者为前置
pub(crate) fn src_from_gpt(tag: &str, ss: u64) -> FileSource {
    let mut src = src_from(tag, fixture_gpt(ss));
    ensure_protective_mbr(&mut src).unwrap();
    src
}

/// MBR 测试镜像：原始字节写一个带 55AA 签名与若干分区条目的 LBA0。
/// `entries` 每项 `(type, start_lba, size_lba)`，落在标准 MBR 分区槽位
/// （off = 446 + 16*(i)，type 在 +4、start 在 +8、size 在 +12）。`img_sectors`
/// 决定整盘大小，用于让合法条目通过越界检查
pub(crate) fn src_from_mbr(tag: &str, entries: &[(u8, u32, u32)], img_sectors: u64) -> FileSource {
    let mut data = vec![0u8; (img_sectors * 512) as usize];
    let lba0 = &mut data[..512];
    for (i, (ty, start, size)) in entries.iter().enumerate() {
        let off = 446 + 16 * i;
        lba0[off + 4] = *ty;
        lba0[off + 8..off + 12].copy_from_slice(&start.to_le_bytes());
        lba0[off + 12..off + 16].copy_from_slice(&size.to_le_bytes());
    }
    lba0[510] = 0x55;
    lba0[511] = 0xAA;
    src_from(tag, data)
}

/// 以**原始字节**写入主副本条目（绕过 gptman：它的 write_into 自带 overlap 校验
/// `InvalidPartitionBoundaries`，而盘上真实存在的重叠表正是别的工具写下的字节，
/// 解析层必须能读到它）
pub(crate) fn write_primary_entries(src: &mut FileSource, ss: u64, ents: &[(u64, u64)]) {
    let geom = geom128(ss);
    let parsed: Vec<GPTPartitionEntry> = ents
        .iter()
        .map(|&(s, e)| GPTPartitionEntry {
            partition_type_guid: [0x11; 16],
            unique_partition_guid: [0x22; 16],
            starting_lba: s,
            ending_lba: e,
            attribute_bits: 0,
            partition_name: "".into(),
        })
        .collect();
    let (bytes, crc) = serialize_array(&parsed, &geom);
    src.write_at(2 * ss, &bytes).unwrap();
    let file_last = src.size / ss - 1;
    let h = geo_header(34, file_last - geom.lba_span() - 1, 2);
    let sec = serialize_header(&h, crc, ss).unwrap();
    src.write_at(ss, &sec).unwrap();
}

/// 把 LBA1 主头的 HeaderSize 改成 `hdr_size` 并按新长度重算 CRC；
/// `reserved` 为真时同时把 92..96 的非零保留字节写进去
pub(crate) fn patch_primary_header(mut src: FileSource, hdr_size: u32, reserved: bool) -> FileSource {
    let ss = src.sector_size as usize;
    let mut sec = vec![0u8; ss];
    src.read_at(ss as u64, &mut sec).unwrap();
    sec[12..16].copy_from_slice(&hdr_size.to_le_bytes());
    if reserved {
        sec[92..96].fill(0xAA);
    }
    sec[16..20].fill(0);
    let crc = crc32(&sec[..hdr_size as usize]);
    sec[16..20].copy_from_slice(&crc.to_le_bytes());
    src.write_at(ss as u64, &sec).unwrap();
    src
}

/// 几何校验的直调夹具：128 × 128B 条目 ⇒ 数组跨度 32 扇区（512B 扇区）
pub(crate) const SPAN_128_512: u64 = 32;

pub(crate) fn geo_header(first_usable: u64, last_usable: u64, entry_lba: u64) -> RawHeader {
    RawHeader {
        revision: 0x0001_0000,
        primary_lba: 1,
        backup_lba: 999,
        first_usable_lba: first_usable,
        last_usable_lba: last_usable,
        disk_guid: [0; 16],
        partition_entry_lba: entry_lba,
        number_of_partition_entries: 128,
        size_of_partition_entry: 128,
        header_size: 92,
    }
}

/// 测试用几何：128 条目 × 128B（跨度为 SPAN_128_512），扇区大小由参数给出。
/// 几何是"表允许多大"的唯一来源，因此测试也不再直接传字面量上限
pub(crate) fn geom128(ss: u64) -> EntryArrayGeometry {
    EntryArrayGeometry::new(ss, 128, 128, MAX_ARRAY_BYTES).unwrap()
}

/// 直接把一份自定义几何的主副本写进镜像（绕过 commit_gpt 的规范化和写入序列）
pub(crate) fn write_raw_primary(src: &mut FileSource, mut h: RawHeader, ss: u64) {
    let geom = EntryArrayGeometry::new(ss, h.size_of_partition_entry, h.number_of_partition_entries, MAX_ARRAY_BYTES).unwrap();
    let (bytes, crc) = serialize_array(&[], &geom);
    src.write_at(h.partition_entry_lba * ss, &bytes).unwrap();
    h.header_size = 92;
    let sec = serialize_header(&h, crc, ss).unwrap();
    src.write_at(ss, &sec).unwrap();
}

/// 直接把一份自定义几何的**备份**副本写进镜像（盘尾头 + 其自述的条目数组位置）。
/// 传入的 `h` 须为备份头语义：MyLBA = 盘尾、AltLBA = 1
pub(crate) fn write_raw_backup(src: &mut FileSource, mut h: RawHeader, ss: u64) {
    let geom = EntryArrayGeometry::new(ss, h.size_of_partition_entry, h.number_of_partition_entries, MAX_ARRAY_BYTES).unwrap();
    let (bytes, crc) = serialize_array(&[], &geom);
    src.write_at(h.partition_entry_lba * ss, &bytes).unwrap();
    h.header_size = 92;
    let file_last = src.size / ss - 1;
    let sec = serialize_header(&h, crc, ss).unwrap();
    src.write_at(file_last * ss, &sec).unwrap();
}