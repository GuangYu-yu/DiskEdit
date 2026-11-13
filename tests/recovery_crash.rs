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

/// 一条搬移计划：条目 `(分区号, 源起点, 长度, 位移, 是否 swap)` 与双游标
struct Reloc {
    entries: Vec<(u32, u64, u64, i64, bool)>,
    entry_index: u32,
    chunks_in_entry: u64,
}

struct Fixture {
    size: u64,
    file_id: [u8; 16],
    op: u8,
    commit: u8,
    digest: [u8; 32],
    restore_blocked: bool,
    /// `None` 表示没有搬移计划
    relocation: Option<Reloc>,
    /// 表区预映像：`(first_lba, 原文)`；写后字节取同长零值
    restore: Vec<(u64, Vec<u8>)>,
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
            relocation: None,
            restore: Vec::new(),
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
        match &self.relocation {
            None => out.push(0),
            Some(reloc) => {
                out.push(1);
                out.extend_from_slice(&(reloc.entries.len() as u32).to_le_bytes());
                for &(partition, src_first, len, delta, is_swap) in &reloc.entries {
                    out.extend_from_slice(&partition.to_le_bytes());
                    out.extend_from_slice(&src_first.to_le_bytes());
                    out.extend_from_slice(&len.to_le_bytes());
                    out.extend_from_slice(&delta.to_le_bytes());
                    out.push(u8::from(is_swap));
                }
                out.extend_from_slice(&reloc.entry_index.to_le_bytes());
                out.extend_from_slice(&reloc.chunks_in_entry.to_le_bytes());
                out.extend_from_slice(&512u64.to_le_bytes()); // chunk 字节
                out.push(0); // TailPacked
                out.extend_from_slice(&0u32.to_le_bytes()); // 无扩容步
                out.extend_from_slice(&self.size.to_le_bytes());
                out.extend_from_slice(&512u64.to_le_bytes()); // 扇区大小
            }
        }
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
        out.extend_from_slice(&(self.restore.len() as u32).to_le_bytes());
        for (first_lba, original) in &self.restore {
            let start = out.len();
            out.extend_from_slice(&512u32.to_le_bytes()); // 扇区大小
            out.extend_from_slice(&first_lba.to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes()); // 每段一扇区
            out.extend_from_slice(&(original.len() as u32).to_le_bytes());
            out.extend_from_slice(original);
            out.extend_from_slice(&vec![0u8; original.len()]); // 写入后字节
            let crc = crc32(&out[start..]);
            out.extend_from_slice(&crc.to_le_bytes());
        }
        out.push(0); // no entry with its table step in flight
        out.push(0); // no table repair
        out.push(0); // table fence: pending

        let mut file = vec![0u8; 16];
        file[0..4].copy_from_slice(b"DSK4");
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

    let (code, out, err) = run(&["abandon", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 0, "{out}{err}");
    let (code, out, err) = run(&["new", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 0, "{out}{err}");
}

#[test]
fn a_committed_record_blocks_new_record_creation() {
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

    let (code, out, err) = run(&["abandon", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(!record_path(&image).exists());
    assert!(sidecar(&image).join(".abandoned").exists());
}

#[test]
fn an_unfinished_relocation_is_not_settled_as_already_reverted() {
    // 表区没有预映像、搬移已越过屏障：记录里只剩进度。`AlreadyReverted` 会把这份
    // 唯一副本当终态删掉，故必须拒绝并保留现场。
    let dir = temp_dir("reloc_progress");
    let image = image_with(&dir, 8 * 1024 * 1024);
    let mut fixture = Fixture::image(8 * 1024 * 1024, file_id(&image), 5);
    fixture.relocation =
        Some(Reloc { entries: vec![(1, 2048, 2048, 2048, false)], entry_index: 0, chunks_in_entry: 0 });
    fixture.restore_blocked = true;
    let bytes = fixture.bytes();
    write_record(&image, &bytes);

    let (code, out, err) = run(&["undo", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 3, "{out}{err}");
    assert!(err.contains("re-run `diskedit move`"), "{err}");
    assert_eq!(
        fs::read(record_path(&image)).unwrap(),
        bytes,
        "the only copy of the relocation progress must survive the refusal"
    );
}

#[test]
fn a_written_table_without_a_preimage_is_not_settled_as_already_reverted() {
    // 全部条目已提交、表区预映像还没落：游标已在终点，但盘上数据已搬走、表项仍是旧位。
    // 阶段是 `TableWritten`，按阶段拒绝并把后续步骤指回原命令。
    let dir = temp_dir("table_written_no_preimage");
    let image = image_with(&dir, 8 * 1024 * 1024);
    let mut fixture = Fixture::image(8 * 1024 * 1024, file_id(&image), 5);
    fixture.relocation =
        Some(Reloc { entries: vec![(1, 2048, 2048, 2048, false)], entry_index: 1, chunks_in_entry: 0 });
    fixture.restore_blocked = true;
    let bytes = fixture.bytes();
    write_record(&image, &bytes);

    let (code, out, err) = run(&["undo", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 3, "{out}{err}");
    // 没有表区 preimage 就无从证明已写目的区与旧区间不相交：判据 fail-closed，
    // 走的是与阶段无关的同一拒绝口径
    assert!(err.contains("no replayable copy"), "{err}");
    assert!(err.contains("re-run `diskedit move`"), "{err}");
    assert_eq!(
        fs::read(record_path(&image)).unwrap(),
        bytes,
        "the record is the only account of the relocation progress and must survive"
    );
}

/// 一张两分区 GPT 的预映像：LBA1 主头 + LBA2 条目数组
fn gpt_preimage(partitions: [(u64, u64); 2]) -> Vec<(u64, Vec<u8>)> {
    let mut array = vec![0u8; 4 * 128];
    for (slot, &(start, len)) in partitions.iter().enumerate() {
        array[slot * 128] = 0x01; // 非零类型 GUID：条目已用
        array[slot * 128 + 32..slot * 128 + 40].copy_from_slice(&start.to_le_bytes());
        array[slot * 128 + 40..slot * 128 + 48].copy_from_slice(&(start + len - 1).to_le_bytes());
    }
    let mut header = vec![0u8; 512];
    header[0..8].copy_from_slice(b"EFI PART");
    header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    header[12..16].copy_from_slice(&92u32.to_le_bytes());
    header[24..32].copy_from_slice(&1u64.to_le_bytes()); // 主头在 LBA1
    header[32..40].copy_from_slice(&16383u64.to_le_bytes()); // 备份头在末 LBA
    header[40..48].copy_from_slice(&34u64.to_le_bytes());
    header[48..56].copy_from_slice(&16350u64.to_le_bytes());
    header[56..72].copy_from_slice(&[0x5A; 16]);
    header[72..80].copy_from_slice(&2u64.to_le_bytes()); // 条目数组在 LBA2
    header[80..84].copy_from_slice(&4u32.to_le_bytes());
    header[84..88].copy_from_slice(&128u32.to_le_bytes());
    header[88..92].copy_from_slice(&crc32(&array).to_le_bytes());
    let crc = crc32(&header[..92]);
    header[16..20].copy_from_slice(&crc.to_le_bytes());
    vec![(1, header), (2, array)]
}

/// 块已写、游标未落盘：当前条目的整段目的区都按已写计。目的区压到旧表所指的区间时
/// `undo` 必须拒绝，否则旧布局会被写回并被指向已覆盖的字节
#[test]
fn a_lagging_cursor_over_an_overlapping_move_is_refused_by_undo() {
    let dir = temp_dir("lagging_cursor_blocked");
    let image = image_with(&dir, 8 * 1024 * 1024);
    let mut fixture = Fixture::image(8 * 1024 * 1024, file_id(&image), 5);
    // 旧布局：分区 1 在 2048..6143，分区 2 在 8192..12287
    fixture.restore = gpt_preimage([(2048, 4096), (8192, 4096)]);
    // 分区 2 已搬完并提交；分区 1 的目标区间 4096..8191 压在旧区间上，游标仍记 0 块
    fixture.relocation = Some(Reloc {
        entries: vec![(2, 8192, 4096, 4096, false), (1, 2048, 4096, 2048, false)],
        entry_index: 1,
        chunks_in_entry: 0,
    });
    fixture.restore_blocked = true;
    let bytes = fixture.bytes();
    write_record(&image, &bytes);

    let (code, out, err) = run(&["undo", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 3, "{out}{err}");
    assert!(err.contains("cannot be shown to be clear"), "{err}");
    assert_eq!(fs::read(record_path(&image)).unwrap(), bytes, "the record must survive the refusal");
}

/// swap 条目不拷数据，但表项提交后按新落点 mkswap 重写整段目的区。目的区压在旧区间上时
/// `undo` 只能拒绝：写回旧表会让数据区间指向刚被 swap 头覆盖的字节
#[test]
fn a_swap_destination_over_the_old_ranges_is_refused_by_undo() {
    let dir = temp_dir("swap_destination_blocked");
    let image = image_with(&dir, 8 * 1024 * 1024);
    let mut fixture = Fixture::image(8 * 1024 * 1024, file_id(&image), 5);
    // 旧布局：分区 1 在 2048..6143，分区 2（swap）在 8192..12287
    fixture.restore = gpt_preimage([(2048, 4096), (8192, 4096)]);
    // swap 从 8192 左移 6144：目的 2048..6143 正是旧布局命名的区间
    fixture.relocation = Some(Reloc {
        entries: vec![(2, 8192, 4096, -6144, true)],
        entry_index: 0,
        chunks_in_entry: 0,
    });
    fixture.restore_blocked = true;
    let bytes = fixture.bytes();
    write_record(&image, &bytes);

    let (code, out, err) = run(&["undo", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 3, "{out}{err}");
    assert!(err.contains("cannot be shown to be clear"), "{err}");
    assert_eq!(fs::read(record_path(&image)).unwrap(), bytes, "the record must survive the refusal");
}

/// 同样的落后游标、目的区不碰旧区间时仍然回放：保守窗口不把可回放的搬移一并拒掉
#[test]
fn a_lagging_cursor_clear_of_the_old_ranges_is_still_replayable() {
    let dir = temp_dir("lagging_cursor_clear");
    let image = image_with(&dir, 8 * 1024 * 1024);
    let mut fixture = Fixture::image(8 * 1024 * 1024, file_id(&image), 5);
    // 旧布局：分区 1 在 2048..3071，分区 2 在 8192..12287
    fixture.restore = gpt_preimage([(2048, 1024), (8192, 4096)]);
    // 分区 2 目的区 12288..16383、分区 1 目的区 6144..7167，都不碰旧区间
    fixture.relocation = Some(Reloc {
        entries: vec![(2, 8192, 4096, 4096, false), (1, 2048, 1024, 4096, false)],
        entry_index: 1,
        chunks_in_entry: 0,
    });
    fixture.restore_blocked = true;
    let bytes = fixture.bytes();
    write_record(&image, &bytes);

    let (code, out, err) = run(&["undo", image.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("no destination range the move has written"), "{out}");
    assert!(!record_path(&image).exists(), "a completed rollback clears its record");
}
