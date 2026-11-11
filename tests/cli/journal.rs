//! 旧 journal 的拒绝口径与目标锁。
//!
//! 恢复现场统一落在记录目录内（`<image>.diskedit/record`），旧 journal 与旧 checkpoint
//! 不由任何命令写出，也不参与现场枚举——它们只是"落点上存在的未识别条目"。
//! 等价的状态类覆盖见 `tests/recovery_crash.rs` / `tests/recovery_states.rs` /
//! `tests/recovery_quarantine.rs`，以及 `tests/cli/record.rs` 的三档崩溃注入。
//! 本文件只保留两条：旧 journal 的拒绝口径（不迁移、不回放、不删除），
//! 以及目标锁的序列化与陈旧锁无害。

use super::{sidecar, JOURNAL_SUFFIX};

/// `add` 取绝对 SIZE 与显式类型 GUID，落进第一个可容纳空洞
fn add_1m(img: &str) -> [&str; 8] {
    ["add", img, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]
}

/// 目标锁的落点：镜像旁的记录目录内。测试要"占着锁"，必须按这个名字打开文件；
/// 判据仍是"锁取不取得到"，不是"文件在不在"
fn record_lock(img: &std::path::Path) -> std::path::PathBuf {
    super::record_dir(img).join(super::RECORD_LOCK_NAME)
}

/// 旧 journal 早于记录格式：undo 不回放、不迁移、不删除；abandon 不认领它，
/// 也不拿"空跑即成功"掩盖它存在这件事
#[test]
fn a_legacy_journal_is_refused_and_left_in_place() {
    let dir = std::env::temp_dir().join(format!("diskedit_lj_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("l.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let journal = sidecar(&dir, "l.img", JOURNAL_SUFFIX);
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = std::process::Command::new(exe).args(args).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let (c, _, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");
    // 旧 journal 的形态：落点上有条目，但它不是记录
    std::fs::write(&journal, b"DEJL\x07 not a recovery record").unwrap();
    let before = std::fs::read(&img).unwrap();

    let (c, _, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 3, "a legacy journal is not undoable: {e}");
    assert!(e.contains("legacy undo journal"), "the refusal must name the artifact: {e}");
    assert!(e.contains("manual handling"), "and say that this build leaves it alone: {e}");
    assert_eq!(std::fs::read(&img).unwrap(), before, "a refused undo must not write");
    assert!(journal.exists(), "the legacy journal must be left in place");

    let (c, o, e) = run(&["abandon", img_s]);
    assert_eq!(c, 0, "abandoning a clean target must be a no-op: {e}");
    assert!(o.contains("nothing to abandon"), "{o}");
    assert!(
        o.contains("legacy undo journal") && o.contains("handle it by hand"),
        "the no-op must disclose the artifact it will not release: {o}"
    );
    assert!(journal.exists(), "abandon must not delete a legacy journal");
    assert_eq!(std::fs::read(&img).unwrap(), before, "abandon must not touch a byte of the target");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 一个目标一把锁。正面：别的持有者还在时写命令必须拒绝，而不是与它并行改同一块盘。
/// 反面同样重要：**残留的锁文件不构成任何阻挡**——判据是"锁取不取得到"，不是"文件在不在"。
/// 锁文件一定会残留，因为释放时删除它有竞态（另一个进程可能刚取到同一把锁）
#[test]
fn target_lock_serializes_writers_and_a_stale_lock_file_is_harmless() {
    let dir = std::env::temp_dir().join(format!("diskedit_tl_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("t.img");
    std::fs::write(&img, vec![0u8; 8 * 1024 * 1024]).unwrap();
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let img_s = img.to_str().unwrap();
    let run = |args: &[&str]| -> (i32, String, String) {
        let out = std::process::Command::new(exe).args(args).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    let (c, _, e) = run(&["new", img_s, "--yes"]);
    assert_eq!(c, 0, "{e}");

    // 另一个持有者占着锁（不同的进程/句柄）：写命令必须拒绝并说清成因
    let lock = record_lock(&img);
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)
        .unwrap();
    holder.try_lock().unwrap();
    let (c, _, e) = run(&add_1m(img_s));
    assert_eq!(c, 4, "a held target lock must stop another writer: {e}");
    assert!(e.contains("another diskedit"), "the refusal must name the cause: {e}");

    // 反面：只读命令**不取所有权**，因此不被别人持有的锁挡住——`info` / `plan` 恰恰
    // 可能被用来查看一块正被写入的盘。（写命令里"先只读看一眼再取锁"的阶段同理）
    let (c, _, e) = run(&["info", img_s]);
    assert_eq!(c, 0, "a read-only command must not be blocked by a held lock: {e}");
    drop(holder);

    // 锁已释放、文件仍在：必须照常工作
    assert!(lock.exists(), "the lock file persists by design");
    let (c, _, e) = run(&add_1m(img_s));
    assert_eq!(c, 0, "a released lock must not block anything: {e}");

    let _ = std::fs::remove_dir_all(&dir);
}
