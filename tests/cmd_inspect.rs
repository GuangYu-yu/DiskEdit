//! `inspect` 的进程级测试：命令结构、证据落点与退出码。
//!
//! 记录字节由本文件按 `src/recovery/record.rs` 的落盘布局构造；解码端的固定头与
//! 载荷 CRC 若漂移，这里会先以 3 暴露出来，而不是静默放过。

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
    let dir = std::env::temp_dir().join(format!("diskedit_inspect_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn image_with(dir: &Path, bytes: usize) -> PathBuf {
    let img = dir.join("disk.img");
    std::fs::write(&img, vec![0u8; bytes]).unwrap();
    img
}

#[cfg(target_os = "linux")]
const HEADER_LEN: usize = 16;

#[cfg(target_os = "linux")]
fn crc32(bytes: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(bytes)
}

/// 一份镜像记录的最小载荷：固定截图字段 + 可变的提交/留存状态与文件 ID
#[cfg(target_os = "linux")]
fn payload(file_id: [u8; 16], commit: u8, retention: u8) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0x11; 16]);
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
    out.extend_from_slice(&0u32.to_le_bytes()); // restore_count: no written ranges
    out.push(0); // no entry with its table step in flight
    out.push(0); // no table repair
    out.push(0); // table fence: pending
    out
}

#[cfg(target_os = "linux")]
fn record(file_id: [u8; 16], commit: u8, retention: u8) -> Vec<u8> {
    let payload = payload(file_id, commit, retention);
    let mut out = vec![0u8; HEADER_LEN];
    out[0..4].copy_from_slice(b"DSK4");
    out[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    out[8..12].copy_from_slice(&crc32(&payload).to_le_bytes());
    let header_crc = crc32(&out[0..12]);
    out[12..16].copy_from_slice(&header_crc.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

#[cfg(target_os = "linux")]
fn write_companion(img: &Path, bytes: &[u8]) {
    let mut dir = img.as_os_str().to_os_string();
    dir.push(".diskedit");
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("record"), bytes).unwrap();
}

#[test]
fn inspect_takes_no_sector_size_and_no_partition_suffix() {
    let dir = temp_dir("usage");
    let img = image_with(&dir, 4096);
    let img_s = img.to_str().unwrap();
    let suffix = format!("{img_s}:1");
    let cases: [&[&str]; 3] = [
        &["inspect"],
        &["inspect", img_s, "--sector-size", "512"],
        &["inspect", suffix.as_str()],
    ];
    for args in cases {
        let (code, _out, err) = run(args);
        assert_eq!(code, 2, "{args:?}: {err}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 目标不存在时的拒绝只在 Linux 上可观察：非 Linux 先由能力门以 6 结束
#[cfg(target_os = "linux")]
#[test]
fn missing_target_is_a_refusal() {
    let dir = temp_dir("missing");
    let img = dir.join("absent.img");
    let (code, _out, err) = run(&["inspect", img.to_str().unwrap()]);
    assert_eq!(code, 3, "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn file_id(path: &Path) -> [u8; 16] {
        let meta = std::fs::metadata(path).unwrap();
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&meta.dev().to_be_bytes());
        id[8..].copy_from_slice(&meta.ino().to_be_bytes());
        id
    }

    /// 目标没有 `<img>.diskedit` 落点：锚点目录打不开，目标/记录读不出 → 3
    #[test]
    fn no_companion_is_three() {
        let dir = temp_dir("absent");
        let img = image_with(&dir, 4096);
        let (code, out, err) = run(&["inspect", img.to_str().unwrap()]);
        assert_eq!(code, 3, "{err}");
        assert!(err.contains("cannot be opened"), "{err}");
        assert!(!out.contains("verdict:"), "no verdict is reached: {out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 锚点目录可读、其中没有记录文件：`Evidence::Absent`，无记录是正常状态
    #[test]
    fn record_missing_from_a_readable_anchor_is_zero() {
        let dir = temp_dir("no-record");
        let img = image_with(&dir, 4096);
        let mut anchor = img.as_os_str().to_os_string();
        anchor.push(".diskedit");
        std::fs::create_dir(PathBuf::from(anchor)).unwrap();
        let (code, out, err) = run(&["inspect", img.to_str().unwrap()]);
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("record: absent"), "{out}");
        assert!(out.contains("verdict: no recovery record"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn committed_record_is_zero() {
        let dir = temp_dir("committed");
        let img = image_with(&dir, 4096);
        write_companion(&img, &record(file_id(&img), 1, 0));
        let (code, out, err) = run(&["inspect", img.to_str().unwrap()]);
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("commit: Committed"), "{out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unverified_record_is_annotated_without_changing_the_code() {
        let dir = temp_dir("unverified");
        let img = image_with(&dir, 4096);
        write_companion(&img, &record(file_id(&img), 1, 2));
        let (code, out, err) = run(&["inspect", img.to_str().unwrap()]);
        assert_eq!(code, 0, "{err}");
        assert!(out.contains("retention: Unverified"), "{out}");
        assert!(err.contains("unverified"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prepared_record_is_zero_with_guidance() {
        let dir = temp_dir("prepared");
        let img = image_with(&dir, 4096);
        write_companion(&img, &record(file_id(&img), 0, 0));
        let (code, out, err) = run(&["inspect", img.to_str().unwrap()]);
        assert_eq!(code, 0, "{out}\n{err}");
        assert!(err.contains("diskedit undo") && err.contains("diskedit abandon"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn foreign_identity_is_three() {
        let dir = temp_dir("foreign");
        let img = image_with(&dir, 4096);
        write_companion(&img, &record([0xEE; 16], 1, 0));
        let (code, _out, err) = run(&["inspect", img.to_str().unwrap()]);
        assert_eq!(code, 3, "{err}");
        assert!(err.contains("different target"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupted_payload_crc_is_three() {
        let dir = temp_dir("crc");
        let img = image_with(&dir, 4096);
        let mut bytes = record(file_id(&img), 1, 0);
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF; // 改动载荷末字节，头 CRC 仍然有效
        write_companion(&img, &bytes);
        let (code, _out, err) = run(&["inspect", img.to_str().unwrap()]);
        assert_eq!(code, 3, "{err}");
        assert!(err.contains("CRC"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncated_record_is_three() {
        let dir = temp_dir("truncated");
        let img = image_with(&dir, 4096);
        write_companion(&img, &record(file_id(&img), 1, 0)[..HEADER_LEN - 12]);
        let (code, _out, err) = run(&["inspect", img.to_str().unwrap()]);
        assert_eq!(code, 3, "{err}");
        assert!(err.contains("shorter"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directory_at_the_evidence_path_is_three() {
        let dir = temp_dir("dir");
        let img = image_with(&dir, 4096);
        let mut path = img.as_os_str().to_os_string();
        path.push(".diskedit");
        let anchor = PathBuf::from(path);
        std::fs::create_dir(&anchor).unwrap();
        std::fs::create_dir(anchor.join("record")).unwrap();
        let (code, _out, err) = run(&["inspect", img.to_str().unwrap()]);
        assert_eq!(code, 3, "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
