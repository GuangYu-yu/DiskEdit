//! 测试夹具：在 `target/fixtures/s1` 下生成可复现的镜像。
//!
//! 只用纯字节构造，不依赖宿主的分区工具；两个测试二进制与基线二进制看到同一现场。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use diskedit_core::table::crc32;

/// 同一进程内多个测试可能同时准备夹具；临时文件名必须互不冲突。
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 先写临时文件再改名：并发读者只会看到完整夹具。
fn write_atomic(path: &Path, bytes: impl AsRef<[u8]>) {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
    std::fs::write(&temporary, bytes.as_ref()).expect("夹具临时文件可写");
    std::fs::rename(&temporary, path).expect("夹具可改名");
}

const SECTOR: usize = 512;
const TOTAL_SECTORS: usize = 8192; // 4 MiB

/// 夹具目录；两个测试二进制与基线二进制看到同一现场。
pub const BASE: &str = "target/fixtures/s1";

/// 写命令用的空白/占用镜像目录。
pub const S2_BASE: &str = "target/fixtures/s2";

/// 建好全部夹具，返回基准目录。
pub fn prepare() -> PathBuf {
    let base = PathBuf::from(BASE);
    std::fs::create_dir_all(&base).expect("夹具目录可建");
    let gpt = image(&base, "gpt.img");
    write_gpt(&gpt, false);
    let ext = image(&base, "ext.img");
    write_gpt(&ext, true);
    let mbr = image(&base, "mbr.img");
    write_mbr(&mbr);
    let empty = image(&base, "empty.img");
    write_atomic(&empty, vec![0u8; 1 << 20]);
    let damaged = image(&base, "damaged.img");
    write_damaged_gpt(&damaged);
    write_atomic(&base.join("tiny.img"), b"hello");
    write_atomic(&base.join("zero.img"), []);
    prepare_write_fixtures();
    base
}

/// 写命令夹具：4 MiB 空白镜像与在 LBA8 起有非零字节的镜像。
fn prepare_write_fixtures() {
    let base = PathBuf::from(S2_BASE);
    std::fs::create_dir_all(&base).expect("写夹具目录可建");
    let blank = vec![0u8; TOTAL_SECTORS * SECTOR];
    write_atomic(&base.join("blank.img"), &blank);
    let mut data = blank;
    for (index, byte) in data[4096..4096 + 5000].iter_mut().enumerate() {
        *byte = b"hello"[index % 5];
    }
    write_atomic(&base.join("data.img"), &data);
}

fn image(base: &Path, name: &str) -> PathBuf {
    base.join(name)
}

fn write_gpt(path: &Path, with_ext: bool) {
    let mut bytes = vec![0u8; TOTAL_SECTORS * SECTOR];
    // 保护性 MBR
    bytes[446 + 4] = 0xEE;
    bytes[446 + 1] = 0x00;
    bytes[446 + 2] = 0x02;
    bytes[446 + 3] = 0x00;
    bytes[446 + 5..446 + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    put_u32(&mut bytes, 446 + 8, 1);
    put_u32(&mut bytes, 446 + 12, (TOTAL_SECTORS - 1) as u32);
    bytes[510] = 0x55;
    bytes[511] = 0xAA;

    let mut entries = vec![0u8; 128 * 128];
    write_entry(
        &mut entries,
        0,
        2048,
        1024,
        &guid("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
        "EFI",
    );
    write_entry(
        &mut entries,
        1,
        3072,
        1024,
        &guid("0FC63DAF-8483-4772-8E79-3D69D8477DE4"),
        "data",
    );
    let entries_crc = crc32(&entries);
    let disk_guid = guid("13121110-1514-1716-1819-1A1B1C1D1E1F");

    let total = TOTAL_SECTORS as u64;
    let primary = header(1, total - 1, 2, entries_crc, disk_guid);
    bytes[SECTOR..SECTOR + 92].copy_from_slice(&primary);
    bytes[2 * SECTOR..2 * SECTOR + entries.len()].copy_from_slice(&entries);
    let backup_lba = total - 33;
    bytes[backup_lba as usize * SECTOR..backup_lba as usize * SECTOR + entries.len()]
        .copy_from_slice(&entries);
    let backup = header(total - 1, 1, backup_lba, entries_crc, disk_guid);
    bytes[(total as usize - 1) * SECTOR..(total as usize - 1) * SECTOR + 92]
        .copy_from_slice(&backup);

    if with_ext {
        let base = 2048 * SECTOR;
        put_u16(&mut bytes, base + 1080, 0xEF53);
    }
    write_atomic(path, bytes);
}

fn write_damaged_gpt(path: &Path) {
    write_gpt(path, false);
    let mut bytes = std::fs::read(path).expect("镜像可读");
    // 主头与备份头的 CRC 都破坏，但签名仍在。
    bytes[SECTOR + 20] ^= 0xFF;
    let last = (TOTAL_SECTORS - 1) * SECTOR;
    bytes[last + 20] ^= 0xFF;
    write_atomic(path, bytes);
}

fn write_mbr(path: &Path) {
    let mut bytes = vec![0u8; TOTAL_SECTORS * SECTOR];
    bytes[446] = 0x00;
    bytes[446 + 1..446 + 4].copy_from_slice(&[0x01, 0x01, 0x00]);
    bytes[446 + 4] = 0x83;
    bytes[446 + 5..446 + 8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    put_u32(&mut bytes, 446 + 8, 2048);
    put_u32(&mut bytes, 446 + 12, 1024);
    bytes[510] = 0x55;
    bytes[511] = 0xAA;
    write_atomic(path, bytes);
}

fn write_entry(
    entries: &mut [u8],
    index: usize,
    start: u64,
    sectors: u64,
    kind: &[u8; 16],
    name: &str,
) {
    let offset = index * 128;
    entries[offset..offset + 16].copy_from_slice(kind);
    entries[offset + 16] = index as u8 + 1;
    put_u64(entries, offset + 32, start);
    put_u64(entries, offset + 40, start + sectors - 1);
    let encoded: Vec<u8> = name
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    entries[offset + 56..offset + 56 + encoded.len()].copy_from_slice(&encoded);
}

fn header(
    my_lba: u64,
    alt_lba: u64,
    entries_lba: u64,
    entries_crc: u32,
    disk_guid: [u8; 16],
) -> [u8; 92] {
    let mut header = [0u8; 92];
    header[0..8].copy_from_slice(b"EFI PART");
    put_u32(&mut header, 8, 0x0001_0000);
    put_u32(&mut header, 12, 92);
    put_u64(&mut header, 24, my_lba);
    put_u64(&mut header, 32, alt_lba);
    put_u64(&mut header, 40, 34);
    put_u64(&mut header, 48, TOTAL_SECTORS as u64 - 34);
    header[56..72].copy_from_slice(&disk_guid);
    put_u64(&mut header, 72, entries_lba);
    put_u32(&mut header, 80, 128);
    put_u32(&mut header, 84, 128);
    put_u32(&mut header, 88, entries_crc);
    let checksum = crc32(&header);
    put_u32(&mut header, 16, checksum);
    header
}

fn guid(text: &str) -> [u8; 16] {
    let hex: Vec<u8> = text
        .replace('-', "")
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("ASCII"), 16).expect("十六进制")
        })
        .collect();
    let mut bytes = [0u8; 16];
    bytes[0] = hex[3];
    bytes[1] = hex[2];
    bytes[2] = hex[1];
    bytes[3] = hex[0];
    bytes[4] = hex[5];
    bytes[5] = hex[4];
    bytes[6] = hex[7];
    bytes[7] = hex[6];
    bytes[8..].copy_from_slice(&hex[8..]);
    bytes
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
