#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败（临时目录 / 进程 / 设备）
//! `records list/verify/scan` 的进程级验收：JSON 字段、八态映射与退出码。
//!
//! 记录字节按 `src/recovery/record.rs` 的落盘布局构造，与 `cmd_inspect.rs` 同源；
//! 目标身份取镜像的 `(dev, ino)`，这样 sidecar 的目录名还原出的目标与记录头一致。

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit")).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diskedit_records_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn crc32(bytes: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(bytes)
}

fn file_id(path: &Path) -> [u8; 16] {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).unwrap();
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&meta.dev().to_be_bytes());
    id[8..].copy_from_slice(&meta.ino().to_be_bytes());
    id
}

const HEADER_LEN: usize = 16;
const TX_ID: [u8; 16] = [0x11; 16];
const TX_HEX: &str = "11111111111111111111111111111111";

/// 最小载荷：固定截图字段 + 可变的提交/留存状态与载体文件身份
fn payload(file_id: [u8; 16], commit: u8, retention: u8) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&TX_ID);
    out.extend_from_slice(&[0x22; 16]);
    out.extend_from_slice(&1u64.to_le_bytes());
    out.push(2); // OpKind::Add
    out.extend_from_slice(&[0x33; 32]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.push(0); // Progress::fs_phase
    out.push(0); // old_range: none
    out.push(0); // lvm_post: none
    out.push(0); // relocation: none
    out.push(3); // RecoveryAction::InspectOnly
    out.extend_from_slice(&0u64.to_le_bytes());
    out.push(commit);
    out.push(retention);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.push(2); // SceneProof::FileId
    out.push(0); // no mapping
    out.push(2); // IdentityMaterial::Image
    out.push(0); // volume serial absent
    out.extend_from_slice(&file_id);
    out.push(1); // size evidence present
    out.extend_from_slice(&4096u64.to_le_bytes());
    out.push(1); // carrier file identity present
    out.extend_from_slice(&file_id);
    out.push(0); // disk GUID unavailable
    out.extend_from_slice(&[0u8; 16]);
    out.push(0); // restore_blocked: no non-reversible write recorded
    out.extend_from_slice(&0u32.to_le_bytes()); // restore_count
    out
}

fn record(file_id: [u8; 16], commit: u8, retention: u8) -> Vec<u8> {
    let payload = payload(file_id, commit, retention);
    let mut out = vec![0u8; HEADER_LEN];
    out[0..4].copy_from_slice(b"DSK2");
    out[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    out[8..12].copy_from_slice(&crc32(&payload).to_le_bytes());
    let header_crc = crc32(&out[0..12]);
    out[12..16].copy_from_slice(&header_crc.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

fn image_with(dir: &Path) -> PathBuf {
    let img = dir.join("disk.img");
    std::fs::write(&img, vec![0u8; 4096]).unwrap();
    img
}

fn record_dir(img: &Path) -> PathBuf {
    let mut dir = img.as_os_str().to_os_string();
    dir.push(".diskedit");
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_record(img: &Path, bytes: &[u8]) {
    std::fs::write(record_dir(img).join("record"), bytes).unwrap();
}

#[test]
fn list_reports_a_committed_sidecar_record() {
    let dir = temp_dir("list");
    let img = image_with(&dir);
    write_record(&img, &record(file_id(&img), 1, 0));
    let (code, out, err) = run(&["records", "list", img.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains(&format!("\"id\":\"{TX_HEX}\"")), "{out}");
    assert!(out.contains("\"state\":\"valid\""), "{out}");
    assert!(out.contains("\"backend\":\"sidecar\""), "{out}");
    assert!(out.contains("\"commit\":\"committed\""), "{out}");
    assert!(out.contains("\"identity_check\":\"checked\""), "{out}");
    assert!(out.contains("\"registered\":false"), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn list_of_a_target_without_records_is_empty() {
    let dir = temp_dir("list-empty");
    let img = image_with(&dir);
    let (code, out, err) = run(&["records", "list", img.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("\"records\":[]"), "{out}");
    assert!(out.contains("\"note\":"), "the list must say what it does not cover: {out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn prepared_record_is_unfinished_and_listed() {
    let dir = temp_dir("prepared");
    let img = image_with(&dir);
    write_record(&img, &record(file_id(&img), 0, 0));
    let (code, out, err) = run(&["records", "list", img.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("\"state\":\"unfinished\""), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn foreign_identity_is_identity_mismatch() {
    let dir = temp_dir("foreign");
    let img = image_with(&dir);
    write_record(&img, &record([0xEE; 16], 1, 0));
    let (code, out, err) = run(&["records", "list", img.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("\"state\":\"identity_mismatch\""), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn damaged_record_is_damaged_not_dropped() {
    let dir = temp_dir("damaged");
    let img = image_with(&dir);
    let mut bytes = record(file_id(&img), 1, 0);
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    write_record(&img, &bytes);
    let (code, out, err) = run(&["records", "list", img.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("\"state\":\"damaged\""), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn quarantined_entry_is_listed_and_marked() {
    let dir = temp_dir("quarantine");
    let img = image_with(&dir);
    let bytes = record(file_id(&img), 1, 0);
    write_record(&img, &bytes);
    std::fs::write(record_dir(&img).join(".abandoned"), &bytes).unwrap();
    let (code, out, err) = run(&["records", "list", img.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("\"quarantine\":\"abandoned\""), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unknown_format_is_not_listed() {
    let dir = temp_dir("unknown");
    let img = image_with(&dir);
    let mut bytes = record(file_id(&img), 1, 0);
    bytes[0..4].copy_from_slice(b"XXXX");
    write_record(&img, &bytes);
    let (code, out, err) = run(&["records", "list", img.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("\"records\":[]"), "unknown formats stay out of the list: {out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn verify_finds_the_record_it_names() {
    let dir = temp_dir("verify");
    let img = image_with(&dir);
    write_record(&img, &record(file_id(&img), 1, 0));
    let (code, out, err) = run(&["records", "verify", TX_HEX, img.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("\"state\":\"valid\""), "{out}");
    assert!(out.contains("\"identity_check\":\"checked\""), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn verify_of_an_unknown_id_is_three() {
    let dir = temp_dir("verify-missing");
    let img = image_with(&dir);
    write_record(&img, &record(file_id(&img), 1, 0));
    let (code, out, err) = run(&[
        "records",
        "verify",
        "00000000000000000000000000000000",
        img.to_str().unwrap(),
    ]);
    assert_eq!(code, 3, "{out}\n{err}");
    assert!(err.contains("no record with id"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn verify_of_a_target_that_cannot_be_probed_is_four() {
    let dir = temp_dir("verify-absent");
    let img = dir.join("absent.img");
    let (code, _out, err) = run(&["records", "verify", TX_HEX, img.to_str().unwrap()]);
    assert_eq!(code, 4, "{err}");
    assert!(err.contains("not verified"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn scan_of_the_image_directory_reports_the_sidecar() {
    let dir = temp_dir("scan");
    let img = image_with(&dir);
    write_record(&img, &record(file_id(&img), 1, 0));
    let (code, out, err) = run(&["records", "scan", dir.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(out.contains("\"complete\":true"), "{out}");
    assert!(out.contains("\"backend\":\"sidecar\""), "{out}");
    // 扫描没有单个目标可比，身份核对标不可用
    assert!(out.contains("\"identity_check\":\"unavailable\""), "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn scan_of_a_record_directory_is_refused() {
    let dir = temp_dir("scan-record-dir");
    let img = image_with(&dir);
    let rd = record_dir(&img);
    let (code, _out, err) = run(&["records", "scan", rd.to_str().unwrap()]);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("record directory"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn list_takes_a_whole_target_only() {
    let dir = temp_dir("usage");
    let img = image_with(&dir);
    let part = format!("{}:1", img.display());
    let (code, _out, err) = run(&["records", "list", &part]);
    assert_eq!(code, 2, "{err}");
    let (code, _out, err) = run(&["records", "verify", TX_HEX, &part]);
    assert_eq!(code, 2, "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}
