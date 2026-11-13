//! 恢复记录对外可见的八种证据形态、七种列表状态
//! 与 Prepared/Committed 的迁移。
//!
//! 需要 Linux：记录后端（openat / O_NOFOLLOW / 目录相对原语）只在 Linux 编译。
//! 用例只用普通镜像文件，不需要 root。`Unreadable` 依赖权限位把文件变成不可读，
//! root 会绕过它，所以该用例先做运行期探测，root 下直接返回，不在 root 上伪造。
//!
//! 记录字节按当前头布局构造；字段序只是夹具，被测对象是 CLI 的分类与状态。
#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let dir = std::env::temp_dir().join(format!("diskedit_states_{tag}_{}", std::process::id()));
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

/// 八态里，`NotOurs` 的两种常见载体由同一个分类器给出。
enum Scene {
    Image { size: u64, file_id: [u8; 16] },
    Block,
}

struct Fixture {
    scene: Scene,
    op: u8,
    commit: u8,
    retention: u8,
    digest: [u8; 32],
}

impl Fixture {
    fn image(size: u64, file_id: [u8; 16]) -> Self {
        Self {
            scene: Scene::Image { size, file_id },
            op: 2,
            commit: 0,
            retention: 0,
            digest: [0u8; 32],
        }
    }

    fn payload(&self) -> Vec<u8> {
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
        out.push(self.retention);
        out.extend_from_slice(&0u16.to_le_bytes()); // stage count
        match self.scene {
            Scene::Image { size, file_id } => {
                out.push(2); // proof: FileId
                out.push(0); // no mapping
                out.push(2); // material: Image
                out.push(0); // no volume serial
                out.extend_from_slice(&file_id);
                out.push(1);
                out.extend_from_slice(&size.to_le_bytes());
                out.push(1);
                out.extend_from_slice(&file_id);
            }
            Scene::Block => {
                out.push(1); // proof: DeviceId
                out.push(0); // no mapping
                out.push(1); // material: Block
                out.extend_from_slice(&8u32.to_le_bytes());
                out.extend_from_slice(&0u32.to_le_bytes());
                out.push(0); // device id absent
                out.push(0); // size absent
                out.push(0); // file id absent
            }
        }
        out.push(0); // disk guid unavailable
        out.extend_from_slice(&[0u8; 16]);
        out.push(0); // restore not blocked
        out.extend_from_slice(&0u32.to_le_bytes()); // restore entry count
        out.push(0); // no entry with its table step in flight
        out.push(0); // no table repair
        out.push(0); // table fence: pending
        out
    }

    fn bytes(&self) -> Vec<u8> {
        let payload = self.payload();
        let mut out = vec![0u8; 16];
        out[0..4].copy_from_slice(b"DSK4");
        out[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        out[8..12].copy_from_slice(&crc32(&payload).to_le_bytes());
        let header_crc = crc32(&out[0..12]);
        out[12..16].copy_from_slice(&header_crc.to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(bytes)
}

fn reseal_header(bytes: &mut [u8]) {
    let header_crc = crc32(&bytes[0..12]);
    bytes[12..16].copy_from_slice(&header_crc.to_le_bytes());
}

#[test]
fn evidence_absent_is_success() {
    let dir = temp_dir("absent");
    let image = image_with(&dir, 4096);
    fs::create_dir_all(sidecar(&image)).unwrap();

    let (code, out, err) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("record: absent"), "{out}");
}

#[test]
fn evidence_recognized_reports_commit_state() {
    let dir = temp_dir("recognized");
    let image = image_with(&dir, 4096);
    let mut fixture = Fixture::image(4096, file_id(&image));
    fixture.commit = 1;
    write_record(&image, &fixture.bytes());

    let (code, out, err) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("record: recognized"), "{out}");
    assert!(out.contains("commit: Committed"), "{out}");
    assert!(out.contains("action: InspectOnly"), "{out}");
}

#[test]
fn evidence_unrecognized_format_is_a_structure_failure() {
    // UnrecognizedFormat + “没有该格式”→ 3。
    let dir = temp_dir("format");
    let image = image_with(&dir, 4096);

    let mut magic = Fixture::image(4096, file_id(&image)).bytes();
    magic[0..4].copy_from_slice(b"XXXX");
    reseal_header(&mut magic);
    write_record(&image, &magic);
    let (code, out, _) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 3);
    assert!(out.contains("record: unrecognized format"), "{out}");

    // 形状不认识：载荷尾部多一个字节，长度与两段 CRC 都对得上，内容解析不过
    let mut shaped = Fixture::image(4096, file_id(&image)).bytes();
    shaped.push(0);
    let payload_len = (shaped.len() - 16) as u32;
    shaped[4..8].copy_from_slice(&payload_len.to_le_bytes());
    let payload_crc = crc32(&shaped[16..]);
    shaped[8..12].copy_from_slice(&payload_crc.to_le_bytes());
    reseal_header(&mut shaped);
    write_record(&image, &shaped);
    let (code, out, _) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 3);
    assert!(out.contains("record: unrecognized format"), "{out}");
}

#[test]
fn a_record_in_the_previous_layout_is_never_resumed_by_a_write_command() {
    let prepare = |tag: &str, op: u8, size: usize| {
        let dir = temp_dir(tag);
        let image = image_with(&dir, size);
        let mut fixture = Fixture::image(size as u64, file_id(&image));
        fixture.op = op;
        let mut bytes = fixture.bytes();
        bytes[0..4].copy_from_slice(b"DSKR");
        reseal_header(&mut bytes);
        (image, bytes)
    };
    let check = |tag: &str, image: &std::path::Path, bytes: &[u8], before: Vec<u8>, code: i32, out: &str, err: &str| {
        assert_eq!(code, 3, "{tag}: {out}{err}");
        assert!(
            err.contains("uses a layout this build does not recognize"),
            "{tag}: {err}"
        );
        assert!(
            err.contains("`diskedit abandon` is the only way to release it"),
            "{tag}: {err}"
        );
        assert_eq!(fs::read(image).unwrap(), before, "{tag}: the target must not change");
        assert_eq!(fs::read(record_path(image)).unwrap(), bytes, "{tag}: the record is left in place");
    };

    // move：记录在打开写事务时就被判成外来布局，不需要先有分区表
    let (image, bytes) = prepare("old_magic_move", 5, 4096);
    write_record(&image, &bytes);
    let before = fs::read(&image).unwrap();
    let target = format!("{}:1", image.display());
    let (code, out, err) = run(&["move", &target, "--start", "2048", "--yes"]);
    check("old_magic_move", &image, &bytes, before, code, &out, &err);

    // resize --allow-move：锁前的只读阶段要先认出表与分区，故先建一张 GPT 与一个分区；
    // 记录仍是外来布局，写事务打开时同样退出 3。旧 magic 的记录连身份字段都读不出来，
    // 建表后镜像尺寸与记录里的不一致不影响这条路径
    let (image, bytes) = prepare("old_magic_resize", 10, 8 * 1024 * 1024);
    let img = image.display().to_string();
    assert_eq!(run(&["new", &img, "--yes"]).0, 0);
    assert_eq!(run(&["add", &img, "1M", "--yes"]).0, 0);
    write_record(&image, &bytes);
    let before = fs::read(&image).unwrap();
    let target = format!("{img}:1");
    let (code, out, err) = run(&["resize", &target, "grow", "--allow-move", "--yes"]);
    check("old_magic_resize", &image, &bytes, before, code, &out, &err);
}

#[test]
fn evidence_foreign_target_is_refused() {
    let dir = temp_dir("foreign");
    let image = image_with(&dir, 4096);
    let mut other = [0u8; 16];
    other[0] = 0xEE;
    let fixture = Fixture::image(4096, other);
    write_record(&image, &fixture.bytes());

    let (code, out, _) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 3);
    assert!(out.contains("record: foreign target"), "{out}");
    assert!(out.contains("recorded identity:"), "{out}");
    assert!(out.contains("current identity:"), "{out}");
}

#[test]
fn evidence_truncated_and_crc_mismatch_are_distinguished() {
    // Truncated CLI 拼作 incomplete；两者都退 3，且必须区分损坏区域。
    let dir = temp_dir("damaged");
    let image = image_with(&dir, 4096);
    let fixture = Fixture::image(4096, file_id(&image));
    let mut bytes = fixture.bytes();
    bytes.truncate(bytes.len() - 1);
    write_record(&image, &bytes);

    let (code, out, _) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 3);
    assert!(out.contains("record: incomplete"), "{out}");

    let mut flipped = Fixture::image(4096, file_id(&image)).bytes();
    let last = flipped.len() - 1;
    flipped[last] ^= 0xFF;
    write_record(&image, &flipped);
    let (code, out, _) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 3);
    assert!(out.contains("record: crc mismatch"), "{out}");
    assert!(out.contains("region: Payload"), "{out}");

    let mut header_broken = Fixture::image(4096, file_id(&image)).bytes();
    header_broken[10] ^= 0xFF;
    write_record(&image, &header_broken);
    let (code, out, _) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 3);
    assert!(out.contains("record: crc mismatch"), "{out}");
    assert!(out.contains("region: Header"), "{out}");
}

#[test]
fn evidence_not_ours_covers_symlink_and_directory() {
    let dir = temp_dir("not_ours");
    let image = image_with(&dir, 4096);
    fs::create_dir_all(sidecar(&image)).unwrap();

    std::os::unix::fs::symlink(dir.join("elsewhere"), record_path(&image)).unwrap();
    let (code, out, _) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 3);
    assert!(out.contains("record: not ours"), "{out}");
    assert!(out.contains("entry: Symlink"), "{out}");

    fs::remove_file(record_path(&image)).unwrap();
    fs::create_dir_all(record_path(&image)).unwrap();
    let (code, out, _) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 3);
    assert!(out.contains("record: not ours"), "{out}");
    assert!(out.contains("entry: Directory"), "{out}");
}

#[test]
fn evidence_unreadable_requires_a_non_root_reader() {
    // root 无视权限位，该用例运行期探测后返回。
    let dir = temp_dir("unreadable");
    let image = image_with(&dir, 4096);
    let fixture = Fixture::image(4096, file_id(&image));
    write_record(&image, &fixture.bytes());

    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(record_path(&image), fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(record_path(&image)).is_ok() {
        return;
    }

    let (code, out, _) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 3);
    assert!(out.contains("record: unreadable"), "{out}");
}

#[test]
fn records_list_maps_every_observable_state() {
    // 七态；UnknownFormat 不进 list，用 scan 观察；BackendMismatch 用 scan
    // 在没有 current 的场景下判定。
    let valid = temp_dir("state_valid");
    let image = image_with(&valid, 4096);
    let mut fixture = Fixture::image(4096, file_id(&image));
    fixture.commit = 1;
    write_record(&image, &fixture.bytes());
    let (code, out, err) = run(&["records", "list", image.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"state\":\"valid\""), "{out}");

    let unfinished = temp_dir("state_unfinished");
    let image = image_with(&unfinished, 4096);
    write_record(&image, &Fixture::image(4096, file_id(&image)).bytes());
    let (code, out, _) = run(&["records", "list", image.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("\"state\":\"unfinished\""), "{out}");

    let damaged = temp_dir("state_damaged");
    let image = image_with(&damaged, 4096);
    let mut bytes = Fixture::image(4096, file_id(&image)).bytes();
    bytes.truncate(bytes.len() - 3);
    write_record(&image, &bytes);
    let (code, out, _) = run(&["records", "list", image.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("\"state\":\"damaged\""), "{out}");

    let mismatch = temp_dir("state_mismatch");
    let image = image_with(&mismatch, 4096);
    let mut other = [0u8; 16];
    other[0] = 0xEE;
    write_record(&image, &Fixture::image(4096, other).bytes());
    let (code, out, _) = run(&["records", "list", image.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("\"state\":\"identity_mismatch\""), "{out}");

    let unknown = temp_dir("state_unknown");
    let image = image_with(&unknown, 4096);
    let mut bytes = Fixture::image(4096, file_id(&image)).bytes();
    bytes[0..4].copy_from_slice(b"XXXX");
    reseal_header(&mut bytes);
    write_record(&image, &bytes);
    let (code, out, _) = run(&["records", "list", image.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("\"records\":[]"), "UnknownFormat must not be listed: {out}");
    let (code, out, _) = run(&["records", "scan", unknown.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("\"state\":\"unknown_format\""), "{out}");

    let backend = temp_dir("state_backend");
    let image = image_with(&backend, 4096);
    let fixture = Fixture {
        scene: Scene::Block,
        op: 2,
        commit: 1,
        retention: 0,
        digest: [0u8; 32],
    };
    write_record(&image, &fixture.bytes());
    let (code, out, _) = run(&["records", "scan", backend.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("\"state\":\"backend_mismatch\""), "{out}");
}

#[test]
fn prepared_goes_to_quarantine_and_committed_goes_to_cleanup() {
    let dir = temp_dir("transition");
    let image = image_with(&dir, 4096);
    let prepared = Fixture::image(4096, file_id(&image));
    write_record(&image, &prepared.bytes());

    let (code, out, err) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("commit: Prepared"), "{out}");
    assert!(err.contains("unfinished:"), "{err}");

    let (code, out, err) = run(&["abandon", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("abandoned 1 recovery record(s)"), "{out}");
    assert!(!record_path(&image).exists());
    let quarantined = sidecar(&image).join(".abandoned");
    assert_eq!(fs::read(&quarantined).unwrap(), prepared.bytes());

    let (code, out, _) = run(&["inspect", image.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("record: absent"), "{out}");
    let (code, out, err) = run(&["inspect", quarantined.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("state: quarantined (abandoned)"), "{out}");
    assert!(err.contains("quarantined:"), "{err}");

    let committed_dir = temp_dir("transition_committed");
    let committed_image = image_with(&committed_dir, 4096);
    let mut committed = Fixture::image(4096, file_id(&committed_image));
    committed.commit = 1;
    write_record(&committed_image, &committed.bytes());

    let (code, out, _) = run(&["inspect", committed_image.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("commit: Committed"), "{out}");

    // 已提交记录：本次只释放记录、不回滚任何区间。记录文件被移走是持久状态变化，
    // 因此不是 NoMutation（4）而是 Partial（5），文案必须写清这一点
    let (code, out, err) = run(&["undo", committed_image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 5, "{out}{err}");
    assert!(err.contains("already committed"), "{err}");
    assert!(err.contains("no recorded range was rolled back"), "{err}");
    assert!(err.contains("the recovery record was removed"), "{err}");
    assert!(!record_path(&committed_image).exists());
    let (code, out, _) = run(&["inspect", committed_image.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(out.contains("record: absent"), "{out}");
}
