//! 单条恢复记录（`<image>.diskedit/record`）的崩溃注入验收：记录落盘、数据改写、提交三档。
//!
//! 三条表类写命令（`new` / `add` / `delete`）已不再写旧 journal。这里同时钉住"禁双写"：
//! 任一条崩溃档位都不得出现 `.diskedit.journal`，记录的落点只有记录目录内的 `record`。

use super::{sidecar, JOURNAL_SUFFIX};
use std::path::{Path, PathBuf};

const RECORD_DIR_SUFFIX: &str = ".diskedit";
const RECORD_FILE_NAME: &str = "record";

fn run(args: &[&str]) -> (i32, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit")).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn run_fault(fault: &str, args: &[&str]) -> (i32, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .env("DISKEDIT_FAULT", fault)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn record_path(img: &Path) -> PathBuf {
    let mut name = img.file_name().unwrap().to_os_string();
    name.push(RECORD_DIR_SUFFIX);
    img.parent().unwrap().join(name).join(RECORD_FILE_NAME)
}

/// 32MiB 镜像 + 一张 GPT（`new` 成功后目标上没有记录）
fn stage(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("diskedit_rec_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("r.img");
    std::fs::write(&img, vec![0u8; 32 * 1024 * 1024]).unwrap();
    let img_s = img.to_str().unwrap();
    let (c, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "the fixture table must be created: {e}");
    let record = record_path(&img);
    let journal = sidecar(&dir, "r.img", JOURNAL_SUFFIX);
    assert!(!record.exists(), "a finished command leaves no record: {e}");
    assert!(!journal.exists(), "table commands must not create a legacy journal: {e}");
    (img, record, journal)
}

fn partition_count(img: &Path) -> usize {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(["info", img.to_str().unwrap()])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    v["partitions"].as_array().map_or(0, Vec::len)
}

fn inspect_code(img: &Path) -> (i32, String) {
    run(&["inspect", img.to_str().unwrap()])
}

/// `add` 取绝对 SIZE，GPT 必须显式给类型 GUID
fn add_1m(img: &str) -> [&str; 8] {
    [
        "add", img, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes",
    ]
}

/// 记录已落盘、目标未改：`undo` 把原文写回（等价于重写同一份字节）并释放记录
#[test]
fn record_persisted_before_data_write() {
    let (img, record, journal) = stage("p");
    let img_s = img.to_str().unwrap();
    let argv = add_1m(img_s);

    let (c, e) = run_fault("record-persisted", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record.exists(), "the record must be durable before any byte is written: {e}");
    assert!(!journal.exists(), "the table path must not write a legacy journal: {e}");
    assert_eq!(partition_count(&img), 0, "no target byte may change before the record lands: {e}");

    let (c, e) = inspect_code(&img);
    assert_eq!(c, 5, "an unfinished record is reported as partial: {e}");
    assert!(e.contains("diskedit undo"), "inspect must name the way out: {e}");

    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "undo must replay the recorded bytes and release the record: {e}");
    assert!(!record.exists(), "a settled record is removed: {e}");
    assert_eq!(partition_count(&img), 0, "{e}");

    let (c, e) = run(&argv);
    assert_eq!(c, 0, "the target must be usable after the rollback: {e}");
    assert_eq!(partition_count(&img), 1, "{e}");
}

/// 数据已改、记录未提交：`undo` 逆序写回原文，表回到写前状态
#[test]
fn record_before_commit_rolls_the_data_back() {
    let (img, record, journal) = stage("b");
    let img_s = img.to_str().unwrap();
    let argv = add_1m(img_s);
    let baseline = std::fs::read(&img).unwrap();

    let (c, e) = run_fault("record-before-commit:1", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record.exists(), "the record must survive the crash: {e}");
    assert!(!journal.exists(), "the table path must not write a legacy journal: {e}");
    // 第一处写入可能只是条目数组（表头未更新前不改变解析结果），判据用原始字节
    assert_ne!(std::fs::read(&img).unwrap(), baseline, "the write must already be on the target: {e}");

    // 现场占用：另一条写命令拒绝，并给出 undo。记录停在 Prepared，
    // 按 Recognized + Provisional 先于"目标忙"判定，落 5
    let (c, e) = run(&add_1m(img_s));
    assert_eq!(c, 5, "another mutator must be refused: {e}");
    assert!(e.contains("undo"), "the refusal must name the rollback: {e}");

    let (c, e) = inspect_code(&img);
    assert_eq!(c, 5, "an unfinished record is reported as partial: {e}");

    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "undo must roll the table write back: {e}");
    assert!(!record.exists(), "{e}");
    assert_eq!(std::fs::read(&img).unwrap(), baseline, "the original bytes must be back: {e}");
    assert_eq!(partition_count(&img), 0, "{e}");
}

/// 已提交、清理未完成：`undo` 只补收尾，不回写数据
#[test]
fn record_committed_is_settled_without_rollback() {
    let (img, record, journal) = stage("c");
    let img_s = img.to_str().unwrap();
    let argv = add_1m(img_s);

    let (c, e) = run_fault("record-committed", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record.exists(), "the committed record must survive the crash: {e}");
    assert!(!journal.exists(), "the table path must not write a legacy journal: {e}");
    assert_eq!(partition_count(&img), 1, "{e}");

    let (c, e) = inspect_code(&img);
    assert_eq!(c, 0, "a committed record is clean: {e}");
    assert!(e.contains("commit: Committed"), "{e}");

    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "undo must release a committed record: {e}");
    assert!(e.contains("nothing was rolled back"), "{e}");
    assert!(!record.exists(), "{e}");
    assert_eq!(partition_count(&img), 1, "a committed table write must stay: {e}");
}

fn first_change(before: &[u8], after: &[u8]) -> Option<usize> {
    before.iter().zip(after).position(|(a, b)| a != b)
}

fn torn_path(record: &Path) -> PathBuf {
    record.parent().unwrap().join(".torn")
}

fn overwrite(img: &Path, at: usize, bytes: &[u8]) {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(img).unwrap();
    f.seek(SeekFrom::Start(at as u64)).unwrap();
    f.write_all(bytes).unwrap();
    f.sync_all().unwrap();
}

/// 外部改动后再 undo：写盘前的盘面核对必须拒绝，记录保留、外来字节保留
#[test]
fn undo_refuses_after_an_external_edit() {
    let (img, record, journal) = stage("x");
    let img_s = img.to_str().unwrap();
    let argv = add_1m(img_s);
    let baseline = std::fs::read(&img).unwrap();

    let (c, e) = run_fault("record-before-commit:1", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record.exists(), "the record must survive the crash: {e}");
    assert!(!journal.exists(), "the table path must not write a legacy journal: {e}");

    // 记录写后的那一段被别的工具改掉：undo 若盲写就会覆盖这笔外来改动
    let crashed = std::fs::read(&img).unwrap();
    let at = first_change(&baseline, &crashed).expect("the crash must leave a written span behind");
    let foreign = vec![0xEEu8; 4096];
    overwrite(&img, at, &foreign);

    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 3, "undo must refuse to replay over an external edit: {e}");
    assert!(e.contains("no longer holds the recorded bytes"), "{e}");
    assert!(record.exists(), "a refused replay must keep the record: {e}");
    assert_eq!(
        &std::fs::read(&img).unwrap()[at..at + foreign.len()],
        &foreign[..],
        "the external edit must survive the refusal: {e}"
    );

    // 拒绝不是死锁：记录里仍有 undo 可回放的范围，故释放要带 --discard-replay
    let (c, e) = run(&["abandon", img_s, "--discard-replay"]);
    assert_eq!(c, 0, "abandon must release the refused record: {e}");
    assert!(!record.exists(), "{e}");
}

/// 镜像增长后再 undo：容量变化不是"同一目标"，不得按旧记录回放
#[test]
fn undo_refuses_after_the_image_grew() {
    let (img, record, _journal) = stage("g");
    let img_s = img.to_str().unwrap();
    let argv = add_1m(img_s);

    let (c, e) = run_fault("record-before-commit:1", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record.exists(), "the record must survive the crash: {e}");
    let crashed = std::fs::read(&img).unwrap();

    let grown = crashed.len() as u64 + 1024 * 1024;
    std::fs::OpenOptions::new().write(true).open(&img).unwrap().set_len(grown).unwrap();

    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 3, "a grown image must not be treated as the recorded target: {e}");
    assert!(e.contains("size changed"), "{e}");
    assert!(record.exists(), "a refused replay must keep the record: {e}");
    assert_eq!(
        std::fs::read(&img).unwrap()[..crashed.len()],
        crashed[..],
        "no recorded byte may be written back: {e}"
    );

    let (c, e) = run(&["abandon", img_s, "--discard-replay"]);
    assert_eq!(c, 0, "abandon must release a record whose size drifted: {e}");
    assert!(!record.exists(), "{e}");
}

/// 记录载荷撕裂：逐条 CRC 通过的完整前缀照常回放，尾部归档，出口 5
#[test]
fn torn_record_replays_the_intact_prefix() {
    let (img, record, _journal) = stage("t");
    let img_s = img.to_str().unwrap();
    let argv = add_1m(img_s);
    let baseline = std::fs::read(&img).unwrap();

    // 第二次目标写入后崩溃：记录里留下两条区间
    let (c, e) = run_fault("record-before-commit:2", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record.exists(), "the record must survive the crash: {e}");
    let crashed = std::fs::read(&img).unwrap();
    let at = first_change(&baseline, &crashed).expect("the crash must leave a written span behind");

    // 截掉尾部 300 字节：最后一条区间不完整，前一条仍可逐条验证
    let len = std::fs::metadata(&record).unwrap().len();
    assert!(len > 600, "the record must hold more than one entry: {len}");
    std::fs::OpenOptions::new().write(true).open(&record).unwrap().set_len(len - 300).unwrap();

    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 5, "a torn record is a partial completion: {e}");
    assert!(e.contains("replayed the 1 verified range(s)"), "{e}");
    assert!(e.contains("archived the torn record as"), "{e}");
    assert!(!record.exists(), "the torn record must leave the slot: {e}");
    assert!(torn_path(&record).exists(), "the torn bytes are archived: {e}");
    assert_eq!(
        std::fs::read(&img).unwrap()[at],
        baseline[at],
        "the verified prefix must be back to its pre-image: {e}"
    );

    // 落点腾空后目标不再被判 busy
    let (c, e) = run(&["inspect", img_s]);
    assert_eq!(c, 0, "no record must remain after the prefix replay: {e}");
    assert!(e.contains("no recovery record"), "{e}");
}

/// 禁双写：成功路径也不得留下旧 journal 或记录
#[test]
fn table_commands_keep_a_single_credential() {
    let (img, record, journal) = stage("s");
    let img_s = img.to_str().unwrap();
    let (c, e) = run(&add_1m(img_s));
    assert_eq!(c, 0, "{e}");
    let (c, e) = run(&["delete", &format!("{img_s}:1"), "--yes"]);
    assert_eq!(c, 0, "{e}");
    assert!(!record.exists(), "a finished command leaves no record: {e}");
    assert!(!journal.exists(), "a finished command leaves no legacy journal: {e}");
}
