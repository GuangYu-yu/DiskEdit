//! 载体对照：同一份在线扩容在来宾里执行。
//!
//! 需要 `DISKEDIT_VM_BUNDLE` 指向一个已组装的 bundle、QEMU 与 `mke2fs`；缺任何一项
//! 时明确回报 `NotApplicable`，不把"没跑"当作通过。

use std::path::{Path, PathBuf};
use std::process::Command;

use diskedit::platform::{OnlineRequest, Platform, host};
use diskedit_core::fs::FsKind;

const SECTOR: u64 = 512;
const PARTITION_START: u64 = 2048;
const PARTITION_SECTORS: u64 = 65536;

#[test]
fn the_carrier_grows_a_mounted_filesystem_without_the_host_capability() {
    let Some(bundle) = std::env::var_os("DISKEDIT_VM_BUNDLE") else {
        eprintln!("NotApplicable: DISKEDIT_VM_BUNDLE is not set");
        return;
    };
    let _ = bundle;
    if Command::new("mke2fs").arg("-V").output().is_err() {
        eprintln!("NotApplicable: mke2fs is not installed");
        return;
    }
    if Command::new("qemu-system-x86_64")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("NotApplicable: qemu-system-x86_64 is not installed");
        return;
    }
    let platform = host();
    let carrier = match platform.carrier() {
        Ok(Some(carrier)) => carrier,
        Ok(None) => {
            eprintln!("NotApplicable: no carrier is configured");
            return;
        }
        Err(error) => {
            eprintln!("NotApplicable: the bundle was rejected: {error}");
            return;
        }
    };

    let image = image();
    let before = ext_blocks(&image).expect("初始 ext 文件系统可读");

    let request = OnlineRequest {
        mount_point: PathBuf::from("/mnt"),
        device: PathBuf::from("/dev/vda1"),
        disk: image.clone(),
        partition: 1,
        filesystem: FsKind::Ext,
        bytes: PARTITION_SECTORS * SECTOR,
        sector_size: SECTOR,
    };
    let report = carrier
        .online_grow(&request)
        .unwrap_or_else(|error| panic!("the carrier failed: {error}"));
    let after = ext_blocks(&image).expect("扩容后的 ext 文件系统可读");
    assert!(
        after > before,
        "the carrier should grow the filesystem: {before} -> {after}"
    );
    let (blocks, block_size) = report
        .blocks
        .zip(report.block_size)
        .expect("the carrier reports the grown geometry");
    assert_eq!(
        blocks, after,
        "the carrier's read-back should match the image"
    );
    assert!(blocks * block_size <= PARTITION_SECTORS * SECTOR);
    std::fs::remove_file(&image).ok();
}

#[test]
fn the_carrier_writes_whole_disk_segments_without_the_host_capability() {
    let Some(bundle) = std::env::var_os("DISKEDIT_VM_BUNDLE") else {
        eprintln!("NotApplicable: DISKEDIT_VM_BUNDLE is not set");
        return;
    };
    let _ = bundle;
    if Command::new("qemu-system-x86_64")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("NotApplicable: qemu-system-x86_64 is not installed");
        return;
    }
    let platform = host();
    let carrier = match platform.carrier() {
        Ok(Some(carrier)) => carrier,
        Ok(None) => {
            eprintln!("NotApplicable: no carrier is configured");
            return;
        }
        Err(error) => {
            eprintln!("NotApplicable: the bundle was rejected: {error}");
            return;
        }
    };
    let image = image();
    let offset = 64 * SECTOR;
    let payload: Vec<u8> = (0..SECTOR).map(|index| (index % 251) as u8).collect();
    let request = diskedit::platform::DiskWriteRequest {
        disk: image.clone(),
        sector_size: SECTOR,
        writes: vec![diskedit::platform::CarrierWrite {
            offset,
            bytes: payload.clone(),
        }],
    };
    carrier
        .write_disk(&request)
        .unwrap_or_else(|error| panic!("the carrier failed: {error}"));
    let bytes = std::fs::read(&image).expect("镜像可读");
    assert_eq!(
        &bytes[offset as usize..offset as usize + payload.len()],
        payload.as_slice(),
        "来宾应把该段字节写到宿主镜像的同一偏移"
    );
    std::fs::remove_file(&image).ok();
}

/// 建一个 32 MiB 分区、里面装着 16 MiB ext4 的 64 MiB 镜像。
fn image() -> PathBuf {
    // 两条用例并行运行；只用 pid 会让它们落到同一个临时镜像上互相改写。
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "diskedit-carrier-{}-{serial}.img",
        std::process::id()
    ));
    std::fs::write(&path, vec![0u8; 64 << 20]).expect("镜像可写");
    let binary = env!("CARGO_BIN_EXE_diskedit");
    run(&[binary, "new", path.to_str().unwrap(), "--yes"]);
    run(&[
        binary,
        "add",
        path.to_str().unwrap(),
        "32M",
        "--start",
        &PARTITION_START.to_string(),
        "--yes",
    ]);
    let status = Command::new("mke2fs")
        .args([
            "-F",
            "-q",
            "-t",
            "ext4",
            "-b",
            "1024",
            "-E",
            &format!("offset={}", PARTITION_START * SECTOR),
        ])
        .arg(&path)
        .arg("16384")
        .status()
        .expect("mke2fs 可运行");
    assert!(status.success(), "mke2fs 应成功");
    path
}

fn run(argv: &[&str]) {
    let output = Command::new(argv[0])
        .args(&argv[1..])
        .output()
        .unwrap_or_else(|error| panic!("运行 {argv:?} 失败: {error}"));
    assert!(
        output.status.success(),
        "{argv:?} 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 从分区里的 ext 超级块读块数；超级块在分区起点后 1024 字节。
fn ext_blocks(image: &Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(image).ok()?;
    file.seek(SeekFrom::Start(PARTITION_START * SECTOR + 1024))
        .ok()?;
    let mut superblock = [0u8; 64];
    file.read_exact(&mut superblock).ok()?;
    if u16::from_le_bytes([superblock[56], superblock[57]]) != 0xEF53 {
        return None;
    }
    Some(u64::from(u32::from_le_bytes([
        superblock[4],
        superblock[5],
        superblock[6],
        superblock[7],
    ])))
}
