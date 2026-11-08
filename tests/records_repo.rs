//! 记录仓库的落点、权限位与扫描上限。
//!
//! 镜像的记录落在 `<img>.diskedit/`，块设备落在 `recovery/<32 位小写十六进制>/`。
//! 本文件钉住目录约束、扫描预算与重复执行的同一性。
//!
//! 需要 Linux：记录后端只在 Linux 编译。涉及宿主仓库根的用例需要 root
//! （`DISKEDIT_STATE_DIR` 仅在 euid=0 时生效），运行期探测不满足时提前返回。

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(args: &[&str]) -> (i32, String, String) {
    run_with_env(args, &[])
}

fn run_with_env(args: &[&str], envs: &[(&str, &Path)]) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_DiskEdit"));
    cmd.args(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("spawn DiskEdit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diskedit_repo_{tag}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn image_with(dir: &Path, name: &str, size: u64) -> PathBuf {
    let image = dir.join(name);
    fs::write(&image, vec![0u8; size as usize]).expect("create image");
    image
}

fn sidecar(image: &Path) -> PathBuf {
    let mut name = image.as_os_str().to_owned();
    name.push(".diskedit");
    PathBuf::from(name)
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("stat").mode() & 0o777
}

fn crc32(bytes: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(bytes)
}

/// 最小记录；扫描只关心结构与承载字节。
fn record_bytes() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&[0x11; 16]);
    p.extend_from_slice(&[0x22; 16]);
    p.extend_from_slice(&1u64.to_le_bytes());
    p.push(2); // OpKind::Add
    p.extend_from_slice(&[0u8; 32]);
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u64.to_le_bytes());
    p.push(0);
    p.push(0);
    p.push(0);
    p.push(0);
    p.push(3); // RecoveryAction::InspectOnly
    p.extend_from_slice(&0u64.to_le_bytes());
    p.push(1); // Committed
    p.push(0);
    p.extend_from_slice(&0u16.to_le_bytes());
    p.push(2); // SceneProof::FileId
    p.push(0);
    p.push(2); // IdentityMaterial::Image
    p.push(0);
    p.extend_from_slice(&[0x33; 16]);
    p.push(1);
    p.extend_from_slice(&4096u64.to_le_bytes());
    p.push(1);
    p.extend_from_slice(&[0x33; 16]);
    p.push(0);
    p.extend_from_slice(&[0u8; 16]);
    p.push(0);
    p.extend_from_slice(&0u32.to_le_bytes());

    let mut out = vec![0u8; 16];
    out[0..4].copy_from_slice(b"DSKR");
    out[4..8].copy_from_slice(&(p.len() as u32).to_le_bytes());
    out[8..12].copy_from_slice(&crc32(&p).to_le_bytes());
    let header_crc = crc32(&out[0..12]);
    out[12..16].copy_from_slice(&header_crc.to_le_bytes());
    out.extend_from_slice(&p);
    out
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// 身份哈希目录名：32 位小写十六进制。
fn identity_hash() -> String {
    "0123456789abcdef".repeat(2)
}

#[test]
fn an_image_keeps_its_records_beside_the_image() {
    // 镜像侧落点是 `<img>.diskedit/`，目录 0700、锁 0600；
    // 成功的写命令会清掉 record。
    let dir = temp_dir("image-layout");
    let image = image_with(&dir, "disk.img", 8 * 1024 * 1024);
    let path = image.to_str().expect("utf-8 path");

    let (code, _, err) = run(&["new", path, "--yes"]);
    assert_eq!(code, 0, "{err}");

    let spot = sidecar(&image);
    assert!(spot.is_dir(), "the record directory must sit beside the image");
    assert_eq!(mode(&spot), 0o700, "record directory mode");
    assert_eq!(mode(&spot.join("record.lock")), 0o600, "lock file mode");
    assert!(!spot.join("record").exists(), "a committed record is cleaned up");

    let (code, out, err) = run(&["records", "list", path]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"records\":[]"), "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_entry_over_sixty_four_mib_stops_the_scan() {
    // 单条记录 64 MiB 上限；超限时整体失败(4)，不给部分结果。
    let dir = temp_dir("scan-limit");
    let image = image_with(&dir, "disk.img", 4096);
    let spot = sidecar(&image);
    fs::create_dir_all(&spot).expect("create record dir");
    let record = spot.join("record");
    fs::File::create(&record)
        .expect("create oversized record")
        .set_len(64 * 1024 * 1024 + 1)
        .expect("size oversized record");

    let (code, out, err) = run(&["records", "scan", dir.to_str().expect("utf-8 path")]);
    assert_eq!(code, 4, "{out}\n{err}");
    let json: serde_json::Value = serde_json::from_str(&out).expect("scan JSON");
    assert_eq!(json["complete"], false, "{out}");
    assert_eq!(json["records"].as_array().expect("records array").len(), 0, "{out}");
    assert!(
        json["reason"].as_str().expect("reason").contains("larger than 64 MiB"),
        "{out}"
    );
    assert!(err.contains("no partial result"), "{err}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_root_inside_the_host_repository_is_refused() {
    // 只接受镜像目录与 `recovery/` 本身；两者内部的路径一律 3。
    if !is_root() {
        return;
    }
    let dir = temp_dir("scan-root");
    let state = dir.join("state");
    let hash = state.join("recovery").join(identity_hash());
    fs::create_dir_all(&hash).expect("create identity hash directory");
    let envs = [("DISKEDIT_STATE_DIR", state.as_path())];

    let (code, _out, err) = run_with_env(&["records", "scan", state.to_str().unwrap()], &envs);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("inside the host recovery repository"), "{err}");

    let dotdot = format!("{}/recovery/..", state.display());
    let (code, _out, err) = run_with_env(&["records", "scan", &dotdot], &envs);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("inside the host recovery repository"), "{err}");

    let (code, _out, err) = run_with_env(&["records", "scan", hash.to_str().unwrap()], &envs);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("is a record directory"), "{err}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_host_repository_lists_identity_hash_directories() {
    // 块设备侧落点是 `recovery/<身份哈希>/`；目录名不是 32 位小写
    // 十六进制的条目不进扫描结果。
    if !is_root() {
        return;
    }
    let dir = temp_dir("host-repo");
    let state = dir.join("state");
    let hash = state.join("recovery").join(identity_hash());
    fs::create_dir_all(&hash).expect("create identity hash directory");
    fs::write(hash.join("record"), record_bytes()).expect("write record");
    let bogus = state.join("recovery/NOT-A-HASH");
    fs::create_dir_all(&bogus).expect("create non-hash directory");
    fs::write(bogus.join("record"), record_bytes()).expect("write record under a bad directory");
    let envs = [("DISKEDIT_STATE_DIR", state.as_path())];

    let root = state.join("recovery");
    let (code, out, err) = run_with_env(&["records", "scan", root.to_str().unwrap()], &envs);
    assert_eq!(code, 0, "{out}\n{err}");
    let json: serde_json::Value = serde_json::from_str(&out).expect("scan JSON");
    assert_eq!(json["complete"], true, "{out}");
    let records = json["records"].as_array().expect("records array");
    assert_eq!(records.len(), 1, "{out}");
    assert_eq!(records[0]["backend"], "host-recovery-repository", "{out}");
    assert!(
        records[0]["path"].as_str().expect("path").starts_with(hash.to_str().expect("utf-8")),
        "{out}"
    );
    // 镜像记录放进块设备仓库：结构能解码，但承载与后端不符。
    assert_eq!(records[0]["state"], "backend_mismatch", "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_refused_repeat_leaves_the_image_and_the_records_untouched() {
    // 同一分区号重复提交被拒；被拒的重复执行不改目标字节，也不改记录目录。
    let dir = temp_dir("repeat");
    let image = image_with(&dir, "disk.img", 8 * 1024 * 1024);
    let path = image.to_str().expect("utf-8 path");
    assert_eq!(run(&["new", path, "--yes"]).0, 0);
    assert_eq!(run(&["add", path, "1M", "--part-number", "1", "--yes"]).0, 0);

    let before = fs::read(&image).expect("read image");
    let spot = sidecar(&image);
    let mut entries: Vec<String> = fs::read_dir(&spot)
        .expect("read record dir")
        .map(|entry| entry.expect("dir entry").file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();

    let (code, _out, err) = run(&["add", path, "1M", "--part-number", "1", "--yes"]);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("already in use"), "{err}");

    let after = fs::read(&image).expect("read image");
    assert_eq!(before, after, "a refused repeat must not touch the image");
    let mut entries_after: Vec<String> = fs::read_dir(&spot)
        .expect("read record dir")
        .map(|entry| entry.expect("dir entry").file_name().to_string_lossy().into_owned())
        .collect();
    entries_after.sort();
    assert_eq!(entries, entries_after, "a refused repeat must not add record files");
    fs::remove_dir_all(&dir).ok();
}

#[cfg(feature = "test-faults")]
#[test]
fn a_persisted_record_is_private() {
    // 记录文件 0600、所在目录 0700。用故障注入停在记录落盘之后。
    let dir = temp_dir("modes");
    let image = image_with(&dir, "disk.img", 8 * 1024 * 1024);
    let path = image.to_str().expect("utf-8 path");

    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(["new", path, "--yes"])
        .env("DISKEDIT_FAULT", "record-persisted")
        .output()
        .expect("spawn DiskEdit");
    assert!(out.status.code().is_none(), "the fault point must abort the process");

    let spot = sidecar(&image);
    let record = spot.join("record");
    assert!(record.is_file(), "the record must be persisted before the fault point");
    assert_eq!(mode(&record), 0o600, "record mode");
    assert_eq!(mode(&spot), 0o700, "record directory mode");

    let (code, listing, err) = run(&["records", "list", path]);
    assert_eq!(code, 0, "{err}");
    assert!(listing.contains("\"state\":\"unfinished\""), "{listing}");
    fs::remove_dir_all(&dir).ok();
}
