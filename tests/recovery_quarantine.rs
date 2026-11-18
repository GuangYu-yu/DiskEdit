//! 隔离区行为。
//!
//! 需要 Linux：记录后端与隔离区的硬链接/目录原语只在 Linux 编译；用例只用普通
//! 镜像文件，不需要 root。记录落在 `<image_state_root>/images/<键摘要>/record`，
//! 隔离条目是同目录的隐藏名；字节只作为夹具。
#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// 本次测试进程的镜像状态根：子进程与测试算出的落点必须一致
fn state_root() -> &'static Path {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("diskedit_quarantine_state_{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        private_dir(&dir);
        dir
    })
}

/// 记录仓库的每一级都按 0700 建立：写命令会复核状态根、`images` 与键目录
fn private_dir(dir: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).unwrap();
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .env("DISKEDIT_STATE_DIR", state_root())
        .output()
        .expect("spawn Diskedit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("diskedit_quarantine_{tag}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn image_with(dir: &Path, size: usize) -> PathBuf {
    let image = dir.join("disk.img");
    fs::write(&image, vec![0u8; size]).unwrap();
    image
}

/// 镜像键目录：`<image_state_root>/images/<键摘要>`，键由文件对象身份派生
fn record_dir(image: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(image).unwrap();
    let key = format!("inode:{}:{}", meta.dev(), meta.ino());
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    state_root().join("images").join(hex)
}

fn record_path(image: &Path) -> PathBuf {
    record_dir(image).join("record")
}

fn write_record(image: &Path, bytes: &[u8]) {
    // 产品把落点建成 0700 并拒绝更松的目录；夹具按同一形态造
    let dir = record_dir(image);
    private_dir(&dir);
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

fn payload(size: u64, file_id: [u8; 16]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0x11u8; 16]); // tx id
    out.extend_from_slice(&[0x22u8; 16]); // plan id
    out.extend_from_slice(&1u64.to_le_bytes()); // plan version
    out.push(2); // OpKind::Add：不可续跑，recover --release 的主用例
    out.extend_from_slice(&[0u8; 32]); // unbound plan digest
    out.extend_from_slice(&0u32.to_le_bytes()); // completed steps
    out.extend_from_slice(&0u64.to_le_bytes()); // transfer cursor
    out.push(0); // fs phase
    out.push(0); // old range absent
    out.push(0); // lvm post absent
    out.push(0); // relocation absent
    out.push(3); // RecoveryAction::InspectOnly
    out.extend_from_slice(&0u64.to_le_bytes());
    out.push(0); // committed: no
    out.push(0); // retention: active
    out.push(2); // proof: FileId
    out.push(0); // no mapping
    out.push(2); // material: Image
    out.push(0); // no volume serial
    out.extend_from_slice(&file_id);
    out.push(1);
    out.extend_from_slice(&size.to_le_bytes());
    out.push(1);
    out.extend_from_slice(&file_id);
    out.push(0); // disk guid unavailable
    out.extend_from_slice(&[0u8; 16]);
    out.push(0); // restore not blocked
    out.extend_from_slice(&0u32.to_le_bytes()); // restore entry count
    out.push(0); // no entry with its table step in flight
    out.push(0); // no table repair
    out.push(0); // table fence: pending
    out
}

fn record_bytes(size: u64, file_id: [u8; 16]) -> Vec<u8> {
    let payload = payload(size, file_id);
    let mut out = vec![0u8; 16];
    out[0..4].copy_from_slice(b"DSK6");
    out[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    out[8..12].copy_from_slice(&crc32(&payload).to_le_bytes());
    let header_crc = crc32(&out[0..12]);
    out[12..16].copy_from_slice(&header_crc.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(bytes)
}

#[test]
fn release_moves_the_record_into_the_quarantine_area() {
    let dir = temp_dir("move");
    let image = image_with(&dir, 4096);
    let bytes = record_bytes(4096, file_id(&image));
    write_record(&image, &bytes);
    let before = fs::read(&image).unwrap();

    let (code, out, err) = run(&["recover", image.to_str().unwrap(), "--release", "--yes"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        out.contains("released 1 recovery record(s) on this target"),
        "{out}"
    );
    assert!(out.contains("rollback is no longer possible"), "{out}");
    assert_eq!(fs::read(&image).unwrap(), before, "recover --release must not touch the target");

    assert!(!record_path(&image).exists());
    let quarantined = record_dir(&image).join(".abandoned");
    assert_eq!(fs::read(&quarantined).unwrap(), bytes);

    let (code, out, _) = run(&["records", "list", image.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("\"quarantine\":\"abandoned\""), "{out}");
    assert!(out.contains("\"state\":\"unfinished\""), "{out}");

    let (code, out, err) = run(&["inspect", quarantined.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("state: quarantined (abandoned)"), "{out}");
    assert!(err.contains("quarantined:"), "{err}");
}

#[test]
fn a_second_release_is_idempotent() {
    // 身份相等的隔离条目恰好一条时重跑不报错（“不是恰好 1 条 → 3”的另一侧）。
    let dir = temp_dir("idempotent");
    let image = image_with(&dir, 4096);
    write_record(&image, &record_bytes(4096, file_id(&image)));

    assert_eq!(run(&["recover", image.to_str().unwrap(), "--release", "--yes"]).0, 0);
    let quarantined = record_dir(&image).join(".abandoned");
    let kept = fs::read(&quarantined).unwrap();

    let (code, out, err) = run(&["recover", image.to_str().unwrap(), "--release", "--yes"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("this enumeration found no unfinished operation"), "{out}");
    assert_eq!(fs::read(&quarantined).unwrap(), kept);
    assert!(!record_dir(&image).join(".abandoned.1").exists());
}

#[test]
fn quarantine_never_overwrites_an_existing_entry() {
    let dir = temp_dir("suffix");
    let image = image_with(&dir, 4096);
    let bytes = record_bytes(4096, file_id(&image));
    write_record(&image, &bytes);

    let old = record_dir(&image).join(".abandoned");
    fs::write(&old, b"earlier evidence").unwrap();

    let (code, out, err) = run(&["recover", image.to_str().unwrap(), "--release", "--yes"]);
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(fs::read(&old).unwrap(), b"earlier evidence");
    assert_eq!(fs::read(record_dir(&image).join(".abandoned.1")).unwrap(), bytes);
}

#[test]
fn release_refuses_a_record_from_another_target() {
    let dir = temp_dir("foreign");
    let image = image_with(&dir, 4096);
    let mut other = [0u8; 16];
    other[0] = 0xEE;
    let bytes = record_bytes(4096, other);
    write_record(&image, &bytes);

    let (code, out, err) = run(&["recover", image.to_str().unwrap(), "--release", "--yes"]);
    assert_eq!(code, 3, "{out}{err}");
    assert!(err.contains("not the target's own recovery scene"), "{err}");
    assert_eq!(fs::read(record_path(&image)).unwrap(), bytes);
    assert!(!record_dir(&image).join(".abandoned").exists());
}

#[test]
fn release_refuses_a_record_in_an_unknown_layout_without_renaming_anything() {
    let dir = temp_dir("foreign_layout");
    let image = image_with(&dir, 4096);
    let mut bytes = record_bytes(4096, file_id(&image));
    bytes[0..4].copy_from_slice(b"DSKR");
    let header_crc = crc32(&bytes[0..12]);
    bytes[12..16].copy_from_slice(&header_crc.to_le_bytes());
    write_record(&image, &bytes);
    // recover --release 取锁会在记录目录里留下 record.lock：先建出来，前后目录条目才可比
    fs::write(record_dir(&image).join("record.lock"), b"").unwrap();
    let entries = |dir: &Path| {
        let mut names: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        names
    };
    let before = entries(&record_dir(&image));

    let (code, out, err) = run(&["recover", image.to_str().unwrap(), "--release", "--yes"]);
    assert_eq!(code, 3, "{out}{err}");
    assert!(err.contains("a layout this build does not recognize"), "{err}");
    assert!(err.contains("will not quarantine it automatically"), "{err}");
    assert!(err.contains("nothing was renamed"), "{err}");
    assert_eq!(entries(&record_dir(&image)), before);
    assert_eq!(fs::read(record_path(&image)).unwrap(), bytes);
}

#[test]
fn release_refuses_a_non_regular_entry() {
    let dir = temp_dir("not_ours");
    let image = image_with(&dir, 4096);
    // 键目录按产品形状（0700）建：本用例要考的是记录落点本身非普通文件
    private_dir(&record_dir(&image));
    let elsewhere = dir.join("elsewhere");
    fs::write(&elsewhere, b"user data").unwrap();
    std::os::unix::fs::symlink(&elsewhere, record_path(&image)).unwrap();

    let (code, out, err) = run(&["recover", image.to_str().unwrap(), "--release", "--yes"]);
    assert_eq!(code, 3, "{out}{err}");
    assert!(err.contains("symbolic link"), "{err}");
    assert!(err.contains("remove that file by hand"), "{err}");
    assert!(record_path(&image).is_symlink());
    assert_eq!(fs::read(&elsewhere).unwrap(), b"user data");
}

#[test]
fn releasing_a_damaged_record_needs_discard_replay() {
    let dir = temp_dir("damaged");
    let image = image_with(&dir, 4096);
    let mut bytes = record_bytes(4096, file_id(&image));
    bytes.truncate(bytes.len() - 2);
    write_record(&image, &bytes);

    let (code, out, err) = run(&["recover", image.to_str().unwrap(), "--release", "--yes"]);
    assert_eq!(code, 3, "{out}{err}");
    assert!(err.contains("--discard-replay"), "{err}");
    assert_eq!(fs::read(record_path(&image)).unwrap(), bytes);

    let (code, out, err) = run(&["recover", image.to_str().unwrap(), "--release", "--yes", "--discard-replay"]);
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(fs::read(record_dir(&image).join(".abandoned")).unwrap(), bytes);
}

#[test]
fn a_renamed_image_still_lists_and_releases_its_old_scene() {
    // 键取文件对象身份：改名/移动不改变键，按新路径拼出的目录就是原名下那个目录，
    // 旧现场因此仍被列出并可被放弃。
    let dir = temp_dir("renamed");
    let image = image_with(&dir, 4096);
    write_record(&image, &record_bytes(4096, file_id(&image)));
    let renamed = dir.join("renamed.img");
    fs::rename(&image, &renamed).unwrap();

    let (code, out, err) = run(&["recover", renamed.to_str().unwrap(), "--release", "--yes"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("released 1 recovery record"), "{out}");
    assert!(record_dir(&renamed).join(".abandoned").exists());
}

#[test]
fn a_sibling_images_record_is_not_part_of_this_target() {
    let dir = temp_dir("sibling");
    let image = image_with(&dir, 4096);
    let other = dir.join("other.img");
    fs::write(&other, vec![0u8; 4096]).unwrap();
    write_record(&other, &record_bytes(4096, file_id(&other)));

    let (code, out, err) = run(&["recover", image.to_str().unwrap(), "--release", "--yes"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("this enumeration found no unfinished operation"), "{out}");
    assert!(record_path(&other).exists(), "another target's scene must be left alone");
}
