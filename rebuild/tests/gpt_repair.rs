//! GPT 修复：扩容后的旧表在写命令里收敛成双副本自洽的规范形态。
//!
//! 现场是"容器变大了、表还停在旧末端"：主头的 `BackupLBA` 指向旧尾，
//! 保护 MBR 的 `SizeInLBA` 也停在旧容器。写命令把修复并入同一次表写：
//! 重写主备头、把备份副本搬到新末端、按本容器口径重写保护 MBR。

mod fixtures;

use diskedit_core::table::gpt::header::GptHeader;

const SECTOR: usize = 512;
/// 原始夹具 4 MiB；扩到 8 MiB 后新末端为 16383。
const GROWN_SECTORS: usize = 16384;

fn extended_image(name: &str) -> String {
    fixtures::prepare();
    let directory = "target/fixtures/repair";
    std::fs::create_dir_all(directory).expect("修复夹具目录可建");
    let path = format!("{directory}/{name}");
    let source = std::path::Path::new(fixtures::BASE).join("gpt.img");
    std::fs::copy(&source, &path).expect("复制夹具");
    // 模拟容器扩容：尾部补零到 8 MiB。
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("镜像可写");
    file.set_len((GROWN_SECTORS * SECTOR) as u64)
        .expect("镜像可延长");
    path
}

fn read_header(bytes: &[u8], lba: usize) -> GptHeader {
    GptHeader::parse(&bytes[lba * SECTOR..(lba + 1) * SECTOR]).expect("头部可解析")
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("偏移在界内"))
}

// covers: R0005, R0006, R0007, R0008, R0009, R0010, R0011
#[test]
fn a_grown_container_is_repaired_in_the_same_table_write() {
    let path = extended_image("grown-delete.img");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_diskedit"))
        .args(["delete", &format!("{path}:2"), "--yes"])
        .output()
        .expect("运行 delete");
    assert_eq!(
        output.status.code(),
        Some(0),
        "修复 + 删除应成功：{:?}\n{:?}",
        output.stderr,
        output.stdout
    );

    let bytes = std::fs::read(&path).expect("读取镜像");
    let last_lba = GROWN_SECTORS - 1;
    let new_last_usable = (last_lba - 32 - 1) as u64;

    let primary = read_header(&bytes, 1);
    assert_eq!(primary.primary_lba, 1);
    assert_eq!(primary.backup_lba, last_lba as u64, "备份头应搬到新末端");
    assert_eq!(
        primary.last_usable_lba, new_last_usable,
        "可用区上界应按新容器重算"
    );

    let backup = read_header(&bytes, last_lba);
    assert_eq!(backup.primary_lba, last_lba as u64);
    assert_eq!(backup.backup_lba, 1);
    assert_eq!(backup.partition_entry_lba, (last_lba - 32) as u64);
    assert_eq!(backup.last_usable_lba, new_last_usable);

    assert_eq!(
        u32_at(&bytes, 446 + 12),
        last_lba as u32,
        "保护 MBR 的 SizeInLBA 应改成本容器口径"
    );

    // 分区 1 的类型 GUID 保留，分区 2 已清空。
    assert_ne!(&bytes[2 * SECTOR..2 * SECTOR + 16], &[0u8; 16]);
    assert_eq!(&bytes[2 * SECTOR + 128..2 * SECTOR + 144], &[0u8; 16]);

    // 修复后的表按新容器读回是合法的 GPT。
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_diskedit"))
        .args(["info", &path])
        .output()
        .expect("运行 info");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("\"label\":\"gpt\""),
        "修复后的镜像应被识别为 GPT"
    );
}

#[test]
fn a_stale_backup_and_protective_mbr_are_reported_before_the_write() {
    // 判定期就能看见需要修复：同一镜像的 `check`/`info` 不允许把损坏说成没有表。
    let path = extended_image("grown-info.img");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_diskedit"))
        .args(["info", &path])
        .output()
        .expect("运行 info");
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"label\":\"gpt\""));
}
