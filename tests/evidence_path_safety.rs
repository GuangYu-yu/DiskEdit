#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败（临时目录 / 进程 / 设备）
//! 证据路径的安全边界。
//!
//! `inspect <path>` 只把直接子项的文件名交给证据后端；绝对路径与 `..` 由内核解析，
//! 不是本层判据。这里钉住可观测的四条：末级必须是普通文件、末级符号链接不跟随、
//! 硬链接不认领、只有记录后端的直接子项才按证据解码。
//!
//! 需要 Linux：记录后端（`openat`/`O_NOFOLLOW`）只在 Linux 编译。

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit")).args(args).output().expect("spawn DiskEdit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 带时限的运行：FIFO 上没有写者时 `open` 会一直等，用例必须在有限时间内收尾。
fn run_bounded(args: &[&str], limit: Duration) -> Option<(i32, String, String)> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn DiskEdit");
    let start = Instant::now();
    loop {
        if child.try_wait().expect("poll DiskEdit").is_some() {
            let out = child.wait_with_output().expect("collect DiskEdit output");
            return Some((
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ));
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diskedit_evidence_{tag}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

const HEADER_LEN: usize = 16;

fn crc32(bytes: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(bytes)
}

/// 最小记录，只用于让「能解码」与「被拒绝」可区分。
fn record_bytes() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&[0x11; 16]); // tx id
    p.extend_from_slice(&[0x22; 16]); // plan id
    p.extend_from_slice(&1u64.to_le_bytes()); // plan version
    p.push(2); // OpKind::Add
    p.extend_from_slice(&[0u8; 32]); // digest
    p.extend_from_slice(&0u32.to_le_bytes()); // completed steps
    p.extend_from_slice(&0u64.to_le_bytes()); // transfer cursor
    p.push(0); // fs phase
    p.push(0); // old range
    p.push(0); // lvm post
    p.push(0); // relocation
    p.push(3); // RecoveryAction::InspectOnly
    p.extend_from_slice(&0u64.to_le_bytes());
    p.push(1); // Committed
    p.push(0); // retention
    p.extend_from_slice(&0u16.to_le_bytes()); // stage count
    p.push(2); // SceneProof::FileId
    p.push(0); // no mapping
    p.push(2); // IdentityMaterial::Image
    p.push(0); // no volume serial
    p.extend_from_slice(&[0x33; 16]);
    p.push(1); // size evidence present
    p.extend_from_slice(&4096u64.to_le_bytes());
    p.push(1); // carrier file identity present
    p.extend_from_slice(&[0x33; 16]);
    p.push(0); // disk guid unavailable
    p.extend_from_slice(&[0u8; 16]);
    p.push(0); // restore not blocked
    p.extend_from_slice(&0u32.to_le_bytes());

    let mut out = vec![0u8; HEADER_LEN];
    out[0..4].copy_from_slice(b"DSK3");
    out[4..8].copy_from_slice(&(p.len() as u32).to_le_bytes());
    out[8..12].copy_from_slice(&crc32(&p).to_le_bytes());
    let header_crc = crc32(&out[0..12]);
    out[12..16].copy_from_slice(&header_crc.to_le_bytes());
    out.extend_from_slice(&p);
    out
}

/// 记录目录没有同名目标时身份核对不可得，证据按结构解码。
fn sidecar(dir: &Path) -> PathBuf {
    let path = dir.join("ghost.diskedit");
    fs::create_dir_all(&path).expect("create record directory");
    path
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("utf-8 temp path")
}

#[test]
fn a_symlinked_record_is_not_followed() {
    let dir = temp_dir("symlink");
    let spot = sidecar(&dir);
    let outside = dir.join("outside.record");
    fs::write(&outside, record_bytes()).expect("write outside record");
    std::os::unix::fs::symlink(&outside, spot.join("record")).expect("create symlink");

    let (code, out, err) = run(&["inspect", path_text(&spot.join("record"))]);
    assert_eq!(code, 3, "{out}\n{err}");
    assert!(out.contains("record: not ours"), "{out}");
    assert!(out.contains("entry: Symlink"), "{out}");
    assert!(!out.contains("transaction:"), "the linked bytes must not be decoded: {out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_hard_linked_record_is_inspected_as_the_same_file() {
    // 别名并发使用不合并、不保证。
    let dir = temp_dir("hardlink");
    let spot = sidecar(&dir);
    let record = spot.join("record");
    fs::write(&record, record_bytes()).expect("write record");
    fs::hard_link(&record, spot.join("record.alias")).expect("create hard link");

    let (code, out, err) = run(&["inspect", path_text(&record)]);
    assert_eq!(code, 0, "a hard link shares the record's identity: {out}\n{err}");
    assert!(out.contains("record: recognized"), "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_directory_entry_is_not_ours() {
    let dir = temp_dir("directory");
    let spot = sidecar(&dir);
    fs::create_dir_all(spot.join("record")).expect("create record directory as entry");

    let (code, out, err) = run(&["inspect", path_text(&spot.join("record"))]);
    assert_eq!(code, 3, "{out}\n{err}");
    assert!(out.contains("record: not ours"), "{out}");
    assert!(out.contains("entry: Directory"), "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_fifo_entry_does_not_block() {
    let dir = temp_dir("fifo");
    let spot = sidecar(&dir);
    let made = Command::new("mkfifo").arg(spot.join("record")).status().expect("run mkfifo");
    assert!(made.success(), "mkfifo must succeed on Linux");

    let outcome = run_bounded(&["inspect", path_text(&spot.join("record"))], Duration::from_secs(5));
    fs::remove_dir_all(&dir).ok();
    let Some((code, out, err)) = outcome else {
        panic!("opening a FIFO record blocked; it must be refused as NotOurs(3)");
    };
    assert_eq!(code, 3, "{out}\n{err}");
    assert!(out.contains("record: not ours"), "{out}");
}

#[test]
fn a_socket_entry_is_unreadable() {
    let dir = temp_dir("socket");
    let spot = sidecar(&dir);
    let listener = UnixListener::bind(spot.join("record")).expect("bind unix socket");

    let (code, out, err) = run(&["inspect", path_text(&spot.join("record"))]);
    drop(listener);
    assert_eq!(code, 4, "{out}\n{err}");
    assert!(out.contains("record: unreadable"), "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_symlinked_anchor_still_checks_the_entry() {
    // 锚点本身按给定路径解析一次；锚点下的末级链接仍在拒绝范围内。
    let dir = temp_dir("anchor");
    let real = dir.join("real");
    fs::create_dir_all(&real).expect("create anchor target");
    fs::write(real.join("record"), record_bytes()).expect("write record");
    std::os::unix::fs::symlink(real.join("record"), real.join("link")).expect("create entry link");
    std::os::unix::fs::symlink(&real, dir.join("ghost.diskedit")).expect("create anchor link");

    let (code, out, err) = run(&["inspect", path_text(&dir.join("ghost.diskedit/link"))]);
    assert_eq!(code, 3, "{out}\n{err}");
    assert!(out.contains("record: not ours"), "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn only_direct_children_of_a_record_directory_are_evidence() {
    let dir = temp_dir("nested");
    let spot = sidecar(&dir);
    fs::create_dir_all(spot.join("sub")).expect("create nested directory");
    fs::write(spot.join("sub/record"), record_bytes()).expect("write nested record");

    let (code, out, err) = run(&["inspect", path_text(&spot.join("sub/record"))]);
    assert_eq!(code, 4, "{out}\n{err}");
    assert!(!out.contains("backend:"), "a nested path is not an evidence path: {out}");
    assert!(!out.contains("record: recognized"), "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_record_lock_file_is_probed_as_a_target() {
    let dir = temp_dir("lock");
    let spot = sidecar(&dir);
    fs::write(spot.join("record.lock"), b"").expect("write lock file");

    let (code, out, err) = run(&["inspect", path_text(&spot.join("record.lock"))]);
    assert_eq!(code, 4, "{out}\n{err}");
    assert!(!out.contains("backend:"), "record.lock is not evidence: {out}");
    assert!(!out.contains("record: recognized"), "{out}");
    fs::remove_dir_all(&dir).ok();
}
