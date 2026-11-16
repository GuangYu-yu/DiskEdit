//! 单条恢复记录（`<image_state_root>/images/<键摘要>/record`）的崩溃注入测试：
//! 记录落盘、数据改写、提交三档。
//!
//! 三条表类写命令（`new` / `add` / `delete`）不写旧 journal。这里同时钉住"禁双写"：
//! 任一条崩溃档位都不得出现 `.diskedit.journal`，记录的落点只有键目录内的 `record`。

use super::{JOURNAL_SUFFIX, record_dir, sidecar};
use std::path::{Path, PathBuf};

const RECORD_FILE_NAME: &str = "record";

fn run(args: &[&str]) -> (i32, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .env("DISKEDIT_STATE_DIR", super::state_root())
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

fn run_fault(fault: &str, args: &[&str]) -> (i32, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .env("DISKEDIT_FAULT", fault)
        .env("DISKEDIT_STATE_DIR", super::state_root())
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
    record_dir(img).join(RECORD_FILE_NAME)
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
        .env("DISKEDIT_STATE_DIR", super::state_root())
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

/// 4 MiB 分区：起始 LBA 2048、结束 LBA 10239，右移时有足够的落脚空间
fn add_4m(img: &str) -> [&str; 8] {
    [
        "add", img, "4M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes",
    ]
}

fn partition_start(img: &Path) -> u64 {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(["info", img.to_str().unwrap()])
        .env("DISKEDIT_STATE_DIR", super::state_root())
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    v["partitions"][0]["first_lba"].as_u64().unwrap()
}

/// 撞键遗留：本目标的记录落点里坐着另一身份的记录时，写命令在取锁前拒绝，
/// 不落锁文件、不写目标字节
#[test]
fn a_record_slot_holding_another_identity_is_refused_before_the_lock() {
    let (a, a_record, _) = stage("f4a");
    let (c, e) = run_fault("record-persisted", &add_1m(a.to_str().unwrap()));
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(a_record.exists(), "the fixture record must land: {e}");

    let (b, b_record, _) = stage("f4b");
    let (c, e) = run_fault("record-persisted", &add_1m(b.to_str().unwrap()));
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(b_record.exists(), "the fixture record must land: {e}");

    // 落点各在自己的键目录里（文件对象身份不同），载荷身份也不同：把 A 的记录搬进 B 的键目录
    std::fs::copy(&a_record, &b_record).unwrap();
    let foreign = std::fs::read(&b_record).unwrap();
    let baseline = std::fs::read(&b).unwrap();
    let lock = b_record.parent().unwrap().join("record.lock");
    let _ = std::fs::remove_file(&lock);

    let (c, e) = run(&add_1m(b.to_str().unwrap()));
    assert_eq!(c, 3, "a record for another identity must be refused: {e}");
    assert!(
        e.contains("the record directory and the target identity do not match"),
        "{e}"
    );
    assert!(!lock.exists(), "the refusal must come before the lock is taken: {e}");
    assert_eq!(std::fs::read(&b).unwrap(), baseline, "no target byte may be written: {e}");
    assert_eq!(std::fs::read(&b_record).unwrap(), foreign, "the foreign record is left as it was: {e}");
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
    assert_eq!(c, 0, "inspect reports an unfinished record as a readable result: {e}");
    assert!(e.contains("diskedit undo"), "inspect must name the way out: {e}");
    assert!(e.contains("diskedit abandon"), "inspect must name the way out: {e}");

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
    assert_eq!(c, 0, "inspect reports an unfinished record as a readable result: {e}");
    assert!(e.contains("diskedit undo"), "inspect must name the way out: {e}");
    assert!(e.contains("diskedit abandon"), "inspect must name the way out: {e}");

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
    assert_eq!(c, 5, "releasing a committed record changes persistent state without rolling anything back: {e}");
    assert!(e.contains("no recorded range was rolled back"), "{e}");
    assert!(e.contains("the recovery record was removed"), "{e}");
    assert!(!record.exists(), "{e}");
    assert_eq!(partition_count(&img), 1, "a committed table write must stay: {e}");
}

/// 搬移的表步已写、记录未提交：目的区压到旧表所指区间时，只回放表会让旧布局指向被覆盖的数据
#[test]
fn undo_refuses_a_move_whose_destinations_overlap_the_old_ranges() {
    let (img, record, _journal) = stage("mvx");
    let img_s = img.to_str().unwrap();
    let part = format!("{img_s}:1");
    let (c, e) = run(&add_4m(img_s));
    assert_eq!(c, 0, "{e}");

    let (c, e) = run_fault("after-entry-commit:0", &["move", &part, "--start", "4096", "--yes"]);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record.exists(), "the record must survive the crash: {e}");
    assert_eq!(partition_start(&img), 4096, "the table step must have landed: {e}");

    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 3, "destinations that cover the old ranges must be refused: {e}");
    assert!(e.contains("finish the relocation"), "the refusal names the two ways out: {e}");
    assert!(record.exists(), "a refused replay keeps the record: {e}");
    assert_eq!(partition_start(&img), 4096, "no table byte may be written back: {e}");
}

/// 目的区与旧区间不相交：只回放表是安全的，但输出必须说清数据没有回滚
#[test]
fn undo_replays_a_move_whose_destinations_stay_clear_of_the_old_ranges() {
    let (img, record, _journal) = stage("mvc");
    let img_s = img.to_str().unwrap();
    let part = format!("{img_s}:1");
    let (c, e) = run(&add_4m(img_s));
    assert_eq!(c, 0, "{e}");

    let (c, e) = run_fault("after-entry-commit:0", &["move", &part, "--start", "10240", "--yes"]);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record.exists(), "the record must survive the crash: {e}");
    assert_eq!(partition_start(&img), 10240, "the table step must have landed: {e}");

    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "a move clear of the old ranges may replay its table: {e}");
    assert!(e.contains("the recorded ranges were not rolled back"), "{e}");
    assert!(
        e.contains("no destination range the move has written, or may have written, overlaps a range that layout names"),
        "{e}"
    );
    assert!(!record.exists(), "{e}");
    assert_eq!(partition_start(&img), 2048, "the table must be back to the old layout: {e}");
}

/// 相交情形只有显式开关才放行；放行输出必须点名旧区间数据不可信
#[test]
fn undo_replays_over_an_overlap_only_with_the_explicit_switch() {
    let (img, record, _journal) = stage("mvf");
    let img_s = img.to_str().unwrap();
    let part = format!("{img_s}:1");
    let (c, e) = run(&add_4m(img_s));
    assert_eq!(c, 0, "{e}");

    let (c, e) = run_fault("after-entry-commit:0", &["move", &part, "--start", "4096", "--yes"]);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record.exists(), "{e}");

    let (c, e) = run(&["undo", img_s, "--yes", "--replay-table-only"]);
    assert_eq!(c, 0, "the explicit switch must let the table replay through: {e}");
    assert!(e.contains("no longer trustworthy"), "a forced replay states the data risk: {e}");
    assert!(!record.exists(), "{e}");
    assert_eq!(partition_start(&img), 2048, "the table must be back to the old layout: {e}");
}

/// 数据搬移中断在表步之前：记录里只有屏障位、没有旧表快照，显式开关也不得放行；
/// 拒绝原因必须说"没有可回放的旧表"，不能说成"已写区间相交"
#[test]
fn replay_table_only_refuses_when_the_record_holds_no_table_snapshot() {
    let (img, record, _journal) = stage("mvn");
    let img_s = img.to_str().unwrap();
    let part = format!("{img_s}:1");
    let (c, e) = run(&add_4m(img_s));
    assert_eq!(c, 0, "{e}");

    let (c, e) = run_fault(
        "chunk:2",
        &["move", &part, "--start", "10240", "--chunk", "1", "--yes"],
    );
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record.exists(), "the record must survive the crash: {e}");
    assert_eq!(partition_start(&img), 2048, "the table step must not have run: {e}");

    let (c, e) = run(&["undo", img_s, "--yes", "--replay-table-only"]);
    assert_eq!(c, 3, "an empty pre-image must not pass the explicit switch: {e}");
    assert!(e.contains("no replayable copy of the old partition table"), "{e}");
    assert!(!e.contains("has already written"), "the reason must not claim an overlap: {e}");
    assert!(record.exists(), "a refused replay keeps the record: {e}");
    assert_eq!(partition_start(&img), 2048, "no table byte may be written back: {e}");
}

fn first_change(before: &[u8], after: &[u8]) -> Option<usize> {
    before.iter().zip(after).position(|(a, b)| a != b)
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
    let (c, e) = run(&["abandon", img_s, "--yes", "--discard-replay"]);
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

    let (c, e) = run(&["abandon", img_s, "--yes", "--discard-replay"]);
    assert_eq!(c, 0, "abandon must release a record whose size drifted: {e}");
    assert!(!record.exists(), "{e}");
}

/// 记录载荷撕裂：整份载荷不再通过长度与 CRC 校验，一个区间都不写回，记录留在原地
#[test]
fn a_torn_record_is_refused_without_writing() {
    let (img, record, _journal) = stage("t");
    let img_s = img.to_str().unwrap();
    let argv = add_1m(img_s);

    // 第二次目标写入后崩溃：记录里留下两条区间
    let (c, e) = run_fault("record-before-commit:2", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record.exists(), "the record must survive the crash: {e}");
    let crashed = std::fs::read(&img).unwrap();

    // 截掉尾部 300 字节：载荷声明的长度不再是文件长度，整份记录不可读
    let len = std::fs::metadata(&record).unwrap().len();
    assert!(len > 600, "the record must hold more than one entry: {len}");
    std::fs::OpenOptions::new().write(true).open(&record).unwrap().set_len(len - 300).unwrap();

    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 3, "a torn record must not be replayed: {e}");
    assert!(e.contains("is not replayable"), "{e}");
    assert!(record.exists(), "a refused replay must keep the record: {e}");
    assert_eq!(
        std::fs::read(&img).unwrap()[..],
        crashed[..],
        "no recorded byte may be written back: {e}"
    );

    // 记录损坏时回滚能力未知，弃权要显式确认
    let (c, e) = run(&["abandon", img_s, "--yes", "--discard-replay"]);
    assert_eq!(c, 0, "abandon must release a torn record: {e}");
    assert!(!record.exists(), "{e}");
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
