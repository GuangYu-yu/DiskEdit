//! 崩溃/中断后的两条出路：同一请求续跑，或只能 `abandon`。
//!
//! 需要 Linux：记录后端只在 Linux 编译；用例只用普通镜像文件，不需要 root。
//! 记录按当前头布局构造；续跑用的计划摘要按“同一请求”的字段算法
//! 复算（OpKind::New + 表种类 + 覆盖开关 + 扇区大小），只作为夹具。
#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .output()
        .expect("spawn Diskedit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diskedit_crash_{tag}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn image_with(dir: &Path, size: usize) -> PathBuf {
    let image = dir.join("disk.img");
    fs::write(&image, vec![0u8; size]).unwrap();
    image
}

fn sidecar(image: &Path) -> PathBuf {
    let mut name = image.as_os_str().to_owned();
    name.push(".diskedit");
    PathBuf::from(name)
}

fn record_path(image: &Path) -> PathBuf {
    sidecar(image).join("record")
}

fn write_record(image: &Path, bytes: &[u8]) {
    let dir = sidecar(image);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("record"), bytes).unwrap();
}

fn file_id(path: &Path) -> [u8; 16] {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).unwrap();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&meta.dev().to_be_bytes());
    out[8..].copy_from_slice(&meta.ino().to_be_bytes());
    out
}

/// 续跑只认同一份请求。`new` 的字段是表种类、覆盖开关、扇区大小。
fn new_digest(sector_size: u64) -> [u8; 32] {
    let mut input = vec![1u8]; // OpKind::New
    for n in [1u64, 0, sector_size] {
        input.extend_from_slice(&n.to_le_bytes());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(&input));
    out
}

struct Fixture {
    size: u64,
    file_id: [u8; 16],
    op: u8,
    commit: u8,
    digest: [u8; 32],
    restore_blocked: bool,
}

impl Fixture {
    fn image(size: u64, file_id: [u8; 16], op: u8) -> Self {
        Self {
            size,
            file_id,
            op,
            commit: 0,
            digest: [0u8; 32],
            restore_blocked: false,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0x11u8; 16]); // tx id
        out.extend_from_slice(&[0x22u8; 16]); // plan id
        out.extend_from_slice(&1u64.to_le_bytes()); // plan version
        out.push(self.op);
        out.extend_from_slice(&self.digest);
        out.extend_from_slice(&0u32.to_le_bytes()); // completed steps
        out.extend_from_slice(&0u64.to_le_bytes()); // transfer cursor
        out.push(0); // fs phase
        out.push(0); // old range absent
        out.push(0); // lvm post absent
        out.push(0); // relocation absent
        out.push(3); // RecoveryAction::InspectOnly
        out.extend_from_slice(&0u64.to_le_bytes());
        out.push(self.commit);
        out.push(0); // retention: active
        out.extend_from_slice(&0u16.to_le_bytes()); // stage count
        out.push(2); // proof: FileId
        out.push(0); // no mapping
        out.push(2); // material: Image
        out.push(0); // no volume serial
        out.extend_from_slice(&self.file_id);
        out.push(1);
        out.extend_from_slice(&self.size.to_le_bytes());
        out.push(1);
        out.extend_from_slice(&self.file_id);
        out.push(0); // disk guid unavailable
        out.extend_from_slice(&[0u8; 16]);
        out.push(u8::from(self.restore_blocked));
        out.extend_from_slice(&0u32.to_le_bytes()); // restore entry count

        let mut file = vec![0u8; 16];
        file[0..4].copy_from_slice(b"DSK2");
        file[4..8].copy_from_slice(&(out.len() as u32).to_le_bytes());
        file[8..12].copy_from_slice(&crc32(&out).to_le_bytes());
        let header_crc = crc32(&file[0..12]);
        file[12..16].copy_from_slice(&header_crc.to_le_bytes());
        file.extend_from_slice(&out);
        file
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(bytes)
}

#[test]
fn the_same_request_continues_a_provisional_new_record() {
    // 可续跑的操作按记录接续；成功后记录清理（→ 0）。
    let dir = temp_dir("resume");
    let image = image_with(&dir, 8 * 1024 * 1024);
    let mut fixture = Fixture::image(8 * 1024 * 1024, file_id(&image), 1);
    fixture.digest = new_digest(512);
    write_record(&image, &fixture.bytes());

    let (code, out, err) = run(&["new", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(!record_path(&image).exists(), "a completed operation must clear its record");

    let (code, out, err) = run(&["info", image.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"label\":\"gpt\""), "{out}");
}

#[test]
fn a_different_request_does_not_continue_the_record() {
    // 摘要不同即另一份请求（摘要与载荷不符）→ 3。
    let dir = temp_dir("different");
    let image = image_with(&dir, 8 * 1024 * 1024);
    let mut fixture = Fixture::image(8 * 1024 * 1024, file_id(&image), 1);
    fixture.digest = [0xAAu8; 32];
    let bytes = fixture.bytes();
    write_record(&image, &bytes);

    let (code, out, err) = run(&["new", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 3, "{out}{err}");
    assert!(err.contains("different `new` request"), "{err}");
    assert_eq!(fs::read(record_path(&image)).unwrap(), bytes);

    let (code, out, err) = run(&["abandon", image.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}{err}");
    let (code, out, err) = run(&["new", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 0, "{out}{err}");
}

#[test]
fn a_committed_record_blocks_new_record_creation() {
    // Committed + 将创建新记录 → 3。
    let dir = temp_dir("committed");
    let image = image_with(&dir, 8 * 1024 * 1024);
    let mut fixture = Fixture::image(8 * 1024 * 1024, file_id(&image), 2);
    fixture.commit = 1;
    let bytes = fixture.bytes();
    write_record(&image, &bytes);

    let (code, out, err) = run(&["new", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 3, "{out}{err}");
    assert!(err.contains("an unfinished operation still owns this target"), "{err}");
    assert_eq!(fs::read(record_path(&image)).unwrap(), bytes);
}

#[test]
fn non_resumable_provisional_is_recovery_pending() {
    // Recognized + Provisional → 5，不改判 3。
    let dir = temp_dir("pending");
    let image = image_with(&dir, 8 * 1024 * 1024);
    let fixture = Fixture::image(8 * 1024 * 1024, file_id(&image), 2);
    write_record(&image, &fixture.bytes());
    let before = fs::read(&image).unwrap();

    let (code, out, err) = run(&["add", image.to_str().unwrap(), "1M", "--yes"]);
    assert_eq!(code, 5, "{out}{err}");
    assert_eq!(fs::read(&image).unwrap(), before, "a blocked command must not write");
    assert!(record_path(&image).exists());
}

#[test]
fn a_blocked_record_leaves_abandon_as_the_only_exit() {
    // 不可逆记录拒绝 undo；abandon 释放现场。
    let dir = temp_dir("only_abandon");
    let image = image_with(&dir, 4096);
    let mut fixture = Fixture::image(4096, file_id(&image), 2);
    fixture.restore_blocked = true;
    let bytes = fixture.bytes();
    write_record(&image, &bytes);

    let (code, out, err) = run(&["undo", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 3, "{out}{err}");
    assert!(err.contains("release the transaction with `diskedit abandon`"), "{err}");
    assert_eq!(fs::read(record_path(&image)).unwrap(), bytes);

    let (code, out, err) = run(&["abandon", image.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(!record_path(&image).exists());
    assert!(sidecar(&image).join(".abandoned").exists());
}
