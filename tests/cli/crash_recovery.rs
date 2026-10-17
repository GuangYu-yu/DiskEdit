use super::{sidecar, CHECKPOINT_SUFFIX, JOURNAL_SUFFIX};

fn run(args: &[&str]) -> (i32, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit")).args(args).output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// 与 `run` 同一命令，但带上注入标签：命中即 abort
fn run_fault(fault: &str, args: &[&str]) -> (i32, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .env("DISKEDIT_FAULT", fault)
        .args(args)
        .output()
        .unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// 32MiB 镜像 + 一个 2048..4095 的分区
fn stage(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("diskedit_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("c.img");
    std::fs::write(&img, vec![0u8; 32 * 1024 * 1024]).unwrap();
    let img_s = img.to_str().unwrap();
    assert_eq!(run(&["new", img_s, "--yes"]).0, 0);
    assert_eq!(run(&["add", img_s, "--start", "2048", "--end", "4095"]).0, 0);
    (img, sidecar(&dir, "c.img", JOURNAL_SUFFIX), sidecar(&dir, "c.img", CHECKPOINT_SUFFIX))
}

/// `resize-part`：数据已搬、表项未提交时崩溃。它留了 ckpt ⇒ 出路是**重跑原命令续跑**
#[test]
fn resize_part_crash_leaves_a_resumable_transaction() {
    let (img, journal, ckpt) = stage("rsc");
    let img_s = img.to_str().unwrap();
    let target = format!("{img_s}:1");
    let argv = ["resize-part", target.as_str(), "--start", "8192", "--end", "10239"];

    let (c, e) = run_fault("rs-before-commit", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(journal.exists(), "the transaction's history must survive the crash: {e}");
    assert!(ckpt.exists(), "a mid-move crash must leave a resumable checkpoint: {e}");
    let len_before = std::fs::metadata(&journal).unwrap().len();

    // 另一条新操作不得接手，且不得动那份 history
    let (c, e) = run(&["add", img_s, "--start", "20480", "--end", "22527"]);
    assert_eq!(c, 30, "another mutator must be refused: {e}");
    assert!(e.contains("owns this target"), "{e}");
    assert!(e.contains("re-run the command that started it"), "with a ckpt the way out is resume: {e}");
    assert_eq!(std::fs::metadata(&journal).unwrap().len(), len_before, "a refusal must not touch the history");

    // 重跑原命令 ⇒ 续跑并收尾
    let (c, e) = run(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    assert!(!journal.exists(), "a finished transaction must be committed: {e}");
    assert!(!ckpt.exists(), "{e}");
}

/// `resize-part` 搬移 NTFS 分区：表项提交后、HiddenSectors 修正前崩溃。
/// 已提交态续跑时盘上几何已是新值，修正与否必须由 ckpt 记录的搬移前起点判定
#[test]
fn ntfs_hidden_sectors_fixed_when_resuming_after_commit() {
    use std::io::{Read, Seek, Write};
    let (img, journal, ckpt) = stage("nhs");
    let img_s = img.to_str().unwrap();
    let target = format!("{img_s}:1");
    let argv = ["resize-part", target.as_str(), "--start", "8192", "--end", "10239"];

    // 手造 NTFS 引导扇区：OEM ID @3 供 identify 认类型，HiddenSectors(u32 LE) @0x1C 记旧起点
    let boot = |hidden: u32| {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x1C..0x20].copy_from_slice(&hidden.to_le_bytes());
        b
    };
    let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
    f.seek(std::io::SeekFrom::Start(2048 * 512)).unwrap();
    f.write_all(&boot(2048)).unwrap();
    drop(f);

    let (c, e) = run_fault("rs-after-commit", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(ckpt.exists(), "a committed-but-unfinished crash leaves the checkpoint: {e}");

    // 中断现场：数据已随表项落在新起点，HiddenSectors 仍是旧值
    let mut buf = [0u8; 512];
    let mut rd = std::fs::File::open(&img).unwrap();
    rd.seek(std::io::SeekFrom::Start(8192 * 512)).unwrap();
    rd.read_exact(&mut buf).unwrap();
    assert_eq!(&buf[3..11], b"NTFS    ", "boot sector must have moved with the data");
    assert_eq!(
        u32::from_le_bytes(buf[0x1C..0x20].try_into().unwrap()),
        2048,
        "the crashed run must leave the old hidden value in place"
    );

    // 续跑：已提交态收尾，HiddenSectors 必须补修到新起点
    let (c, e) = run(&argv);
    assert_eq!(c, 0, "committed resume must finish: {e}");
    rd.seek(std::io::SeekFrom::Start(8192 * 512)).unwrap();
    rd.read_exact(&mut buf).unwrap();
    assert_eq!(
        u32::from_le_bytes(buf[0x1C..0x20].try_into().unwrap()),
        8192,
        "hidden sectors must be corrected to the new start on resume"
    );
    assert!(!ckpt.exists() && !journal.exists(), "a finished transaction must be cleaned up");
}

/// `apply`（尾部紧凑打包）搬移 NTFS 分区：表项 commit 后、HiddenSectors 修正前崩溃。
/// 续跑须重入该分区：chunk 跳过、重提交幂等、修正补齐（apply 路径的同类窗口）
#[test]
fn move_ntfs_hidden_sectors_fixed_when_resuming_after_commit() {
    use std::io::{Read, Seek, Write};
    let (img, journal, ckpt) = stage("mhs");
    let img_s = img.to_str().unwrap();
    let argv = ["apply", img_s, "--grow", "1", "--no-fs"];

    // 挡路分区 2（apply 只搬目标右侧的分区）：手造 NTFS 引导扇区，
    // OEM ID @3 供 identify 认类型，HiddenSectors(u32 LE) @0x1C 记旧起点
    assert_eq!(run(&["add", img_s, "--start", "8192", "--end", "12287"]).0, 0);
    let boot = |hidden: u32| {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x1C..0x20].copy_from_slice(&hidden.to_le_bytes());
        b
    };
    let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
    f.seek(std::io::SeekFrom::Start(8192 * 512)).unwrap();
    f.write_all(&boot(8192)).unwrap();
    drop(f);

    let (c, e) = run_fault("before-hidden-fix", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(ckpt.exists(), "a committed-but-unfinished crash leaves the checkpoint: {e}");

    // 新起点以盘上表为准（尾部打包的位置不硬编码）
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(["info", img_s])
        .output()
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    let new_first = v["partitions"][1]["first_lba"].as_u64().unwrap();
    assert_ne!(new_first, 8192, "the committed table must be at the packed position");

    // 中断现场：数据已随表项落在新起点，HiddenSectors 仍是旧值
    let mut buf = [0u8; 512];
    let mut rd = std::fs::File::open(&img).unwrap();
    rd.seek(std::io::SeekFrom::Start(new_first * 512)).unwrap();
    rd.read_exact(&mut buf).unwrap();
    assert_eq!(&buf[3..11], b"NTFS    ", "boot sector must have moved with the data");
    assert_eq!(
        u32::from_le_bytes(buf[0x1C..0x20].try_into().unwrap()),
        8192,
        "the crashed run must leave the old hidden value in place"
    );

    // 续跑：重入该分区，HiddenSectors 必须补修到新起点
    let (c, e) = run(&argv);
    assert_eq!(c, 0, "resuming must finish the relocation: {e}");
    rd.seek(std::io::SeekFrom::Start(new_first * 512)).unwrap();
    rd.read_exact(&mut buf).unwrap();
    assert_eq!(
        u32::from_le_bytes(buf[0x1C..0x20].try_into().unwrap()),
        new_first as u32,
        "hidden sectors must be corrected to the new start on resume"
    );
    assert!(!ckpt.exists() && !journal.exists(), "a finished transaction must be cleaned up");
}

/// copy 的严格打开：单分区 resize 的中途现场对 `resize` / `copy` 都不可续跑
/// ——`resize` 对它拒绝且出路文案不得误导（重跑 resize 救不了它）；`copy` 没有续跑
/// 能力，绝不静默接管别人的现场（否则成功后会把人家的 journal 当自己的清掉）
#[test]
fn foreign_resize_slot_refuses_resize_and_copy() {
    let (img, journal, ckpt) = stage("frs");
    let img_s = img.to_str().unwrap();
    let target = format!("{img_s}:1");
    let argv = ["resize-part", target.as_str(), "--start", "8192", "--end", "10239"];

    let (c, e) = run_fault("rs-before-commit", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(ckpt.exists(), "a mid-move crash must leave a resumable checkpoint: {e}");
    let len_before = std::fs::metadata(&journal).unwrap().len();

    // resize：拒绝（30），且出路文案给出真正能续跑的命令
    let (c, e) = run(&["resize", target.as_str(), "grow", "--allow-move", "--yes"]);
    assert_eq!(c, 30, "resize must refuse a foreign single-partition job: {e}");
    assert!(e.contains("resize-part") && e.contains("move"), "the way out must name the resuming commands: {e}");
    // copy：同样拒绝，不得接管
    let (c, e) = run(&["copy", target.as_str(), "--start", "end"]);
    assert_eq!(c, 30, "copy must refuse while any job owns the slot: {e}");
    // 两次拒绝都不得动现场
    assert!(ckpt.exists() && std::fs::metadata(&journal).unwrap().len() == len_before, "a refusal must not touch the scene");

    // 重跑原命令收尾（对账：现场本身仍是可续跑的）
    let (c, e) = run(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    assert!(!ckpt.exists() && !journal.exists());
}

/// `copy`：数据已复制、表项未提交时崩溃。它**不写 ckpt**，journal 又已越过不可回滚点
/// ⇒ 既续不了也回滚不了，只有 `abandon` 能释放。这条专门盯住"三路出路给同一句话"的错
#[test]
fn copy_crash_leaves_a_transaction_that_only_abandon_can_release() {
    let (img, journal, ckpt) = stage("cpc");
    let img_s = img.to_str().unwrap();
    let target = format!("{img_s}:1");

    let (c, e) = run_fault("copy-before-commit", &["copy", target.as_str(), "--start", "20480", "--chunk-size", "1"]);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(journal.exists(), "the history must survive the crash: {e}");
    assert!(!ckpt.exists(), "copy leaves no checkpoint to resume from: {e}");

    let (c, e) = run(&["add", img_s, "--start", "28672", "--end", "30719"]);
    assert_eq!(c, 30, "another mutator must be refused: {e}");
    assert!(e.contains("owns this target"), "{e}");
    assert!(
        e.contains("only way to release it"),
        "no ckpt and already past the barrier ⇒ only abandon releases it: {e}"
    );

    // 被推荐的那条出路必须真的走得通：undo 拒绝，abandon 释放
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 10, "undo must refuse a journal that crossed the point of no return: {e}");
    assert!(e.contains("non-reversible"), "{e}");
    let (c, e) = run(&["abandon", img_s, "--yes"]);
    assert_eq!(c, 0, "abandon must release it: {e}");
    assert!(!journal.exists(), "{e}");
    let (c, e) = run(&["add", img_s, "--start", "28672", "--end", "30719"]);
    assert_eq!(c, 0, "the target must be usable again: {e}");
}

/// 64MiB 镜像 + 一条 4MiB 分区（2048..10239），分区数据区填确定性伪随机图案。
/// 返回（镜像、图案）——图案用于搬移后逐字节对账
fn stage_self_overlap(tag: &str) -> (std::path::PathBuf, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("diskedit_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("c.img");
    std::fs::write(&img, vec![0u8; 64 * 1024 * 1024]).unwrap();
    let img_s = img.to_str().unwrap();
    assert_eq!(run(&["new", img_s, "--yes"]).0, 0);
    assert_eq!(run(&["add", img_s, "--start", "2048", "--end", "10239"]).0, 0);
    let (start, len) = (2048u64 * 512, 8192u64 * 512);
    let mut pattern = vec![0u8; len as usize];
    let mut x = 0x1234u32;
    for b in pattern.iter_mut() {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        *b = (x >> 24) as u8;
    }
    let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(start)).unwrap();
    std::io::Write::write_all(&mut f, &pattern).unwrap();
    (img, pattern)
}

/// 自重叠搬移（位移 < 分区长度）中断 → 续跑 → 与原图案逐字节比对。
/// 重做的 chunk 读到的必须是原始源数据：位移小于 chunk 时 chunk 收敛到位移以内，
/// 且自重叠条目停用批量提交——两条保证缺一，中断续跑都会把覆写过的源字节当数据搬走
///
/// `move` 走单分区路径（rs-chunk 故障点），起点按 MiB 对齐，故位移取对齐后的值
#[test]
fn self_overlap_delta_below_chunk_crash_resume_is_byte_exact() {
    let (img, pattern) = stage_self_overlap("sov");
    let img_s = img.to_str().unwrap();
    let ckpt = sidecar(img.parent().unwrap(), "c.img", CHECKPOINT_SUFFIX);
    // 位移 1MiB（--start 4096，已对齐）< chunk 2MiB：chunk 收敛到 1MiB，共 4 块
    let argv = ["move", &format!("{img_s}:1"), "--start", "4096", "--chunk-size", "2"];
    let (c, e) = run_fault("rs-chunk:2", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(ckpt.exists(), "a mid-move crash must leave a resumable checkpoint: {e}");

    let (c, e) = run(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    assert!(!ckpt.exists(), "{e}");
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(&raw[4096 * 512..(4096 + 8192) * 512], &pattern[..], "relocated bytes must match the original pattern");
}

/// 同上，位移（2MiB）大于 chunk（1MiB）：chunk 不收敛，但自重叠仍逐 chunk 推进
/// durable 边界——重做只从最后一个已落盘 chunk 之后开始
#[test]
fn self_overlap_delta_above_chunk_crash_resume_is_byte_exact() {
    let (img, pattern) = stage_self_overlap("sov2");
    let img_s = img.to_str().unwrap();
    let ckpt = sidecar(img.parent().unwrap(), "c.img", CHECKPOINT_SUFFIX);
    // 位移 2MiB（--start 6144，已对齐）> chunk 1MiB，共 4 块
    let argv = ["move", &format!("{img_s}:1"), "--start", "6144", "--chunk-size", "1"];
    let (c, e) = run_fault("rs-chunk:3", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(ckpt.exists(), "a mid-move crash must leave a resumable checkpoint: {e}");

    let (c, e) = run(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    assert!(!ckpt.exists(), "{e}");
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(&raw[6144 * 512..(6144 + 8192) * 512], &pattern[..], "relocated bytes must match the original pattern");
}

/// plan/apply 路径（批量提交）的自重叠回归：挡路分区被搬移的位移（1MiB）小于其
/// 长度（24MiB），chunk 1MiB 共 24 块。批量提交下 durable 边界每 16 块推进一次，
/// 中断在 chunk 17 时重做 chunk 16——其源区间恰被 chunk 17 的写覆盖（位移 = 1 chunk
/// 的自重叠污染窗口）。停用批量后 durable 逐 chunk 推进，重做自 chunk 18 起，必须
/// 逐字节完好
#[test]
fn plan_path_self_overlap_disables_batch_commit() {
    let dir = std::env::temp_dir().join(format!("diskedit_sovb_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("c.img");
    std::fs::write(&img, vec![0u8; 64 * 1024 * 1024]).unwrap();
    let img_s = img.to_str().unwrap();
    assert_eq!(run(&["new", img_s, "--yes"]).0, 0);
    assert_eq!(run(&["add", img_s, "--start", "2048", "--end", "10239"]).0, 0);
    assert_eq!(run(&["add", img_s, "--start", "10240", "--end", "59391"]).0, 0);
    // 挡路者 24MiB（10240..59391），填图案作为对账数据
    let (p2_start, p2_len) = (10240u64 * 512, 49152u64 * 512);
    let mut pattern = vec![0u8; p2_len as usize];
    let mut x = 0xBEEFu32;
    for b in pattern.iter_mut() {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        *b = (x >> 24) as u8;
    }
    let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(p2_start)).unwrap();
    std::io::Write::write_all(&mut f, &pattern).unwrap();
    drop(f);

    // resize #1 +1MiB：#2 被右移 1MiB（< 24MiB ⇒ 自重叠），24 块批量循环
    let argv = ["resize", &format!("{img_s}:1"), "+1M", "--allow-move", "--yes", "--no-fs"];
    let (c, e) = run_fault("chunk:17", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    let ckpt = sidecar(&dir, "c.img", CHECKPOINT_SUFFIX);
    assert!(ckpt.exists(), "a mid-move crash must leave a resumable checkpoint: {e}");

    let (c, e) = run(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    assert!(!ckpt.exists(), "{e}");
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(&raw[(10240 + 2048) * 512..(10240 + 2048 + 49152) * 512], &pattern[..], "relocated bytes must match the original pattern");
}

/// 目标分区右侧紧邻挡路者：`resize grow`（尾部打包）与 `resize SIZE`（最小位移）
/// 各自只有走搬移才能完成，而两条命令共用同一个续跑槽位。按对方的语义执行槽上
/// 那份 plan，就是把请求静默放大或缩水——故必须拒绝，并指回原命令
#[test]
fn pending_relocation_job_refuses_the_other_resize_semantic() {
    let dir = std::env::temp_dir().join(format!("diskedit_sem_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("s.img");
    std::fs::write(&img, vec![0u8; 32 * 1024 * 1024]).unwrap();
    let img_s = img.to_str().unwrap();
    let target = format!("{img_s}:1");
    assert_eq!(run(&["new", img_s, "--yes"]).0, 0);
    assert_eq!(run(&["add", img_s, "--start", "2048", "--end", "4095"]).0, 0);
    assert_eq!(run(&["add", img_s, "--start", "4096", "--end", "6143"]).0, 0);
    let journal = sidecar(&dir, "s.img", JOURNAL_SUFFIX);
    let ckpt = sidecar(&dir, "s.img", CHECKPOINT_SUFFIX);

    // ① 精确 SIZE 作业（最小位移 plan）中断
    // 两个分区都没有 FS：扩分区表要显式 --no-fs（槽位/续跑语义与它无关）
    let size_argv = ["resize", target.as_str(), "+3M", "--allow-move", "--yes", "--no-fs"];
    let (c, e) = run_fault("chunk:1", &size_argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(ckpt.exists(), "a mid-move crash must leave a checkpoint: {e}");

    // grow 语义与它不符：拒绝，且指回当初那条命令
    let grow_argv = ["resize", target.as_str(), "grow", "--allow-move", "--yes", "--no-fs"];
    let (c, e) = run(&grow_argv);
    assert_eq!(c, 10, "grow must refuse a pending SIZE job: {e}");
    assert!(e.contains("`resize SIZE --allow-move`"), "the refusal must name the job it belongs to: {e}");
    assert!(ckpt.exists(), "a refusal must not touch the scene");

    // 重跑原命令 ⇒ 续跑并收尾（此刻 p1 与 p2 已紧邻，grow 只剩搬移一条路）
    let (c, e) = run(&size_argv);
    assert_eq!(c, 0, "re-running the original command must resume and finish: {e}");
    assert!(!ckpt.exists() && !journal.exists(), "{e}");

    // ② 反向：grow 作业（尾部打包 plan）中断 → SIZE 语义必须拒绝
    let (c, e) = run_fault("chunk:1", &grow_argv);
    assert_ne!(c, 0, "{e}");
    assert!(ckpt.exists(), "{e}");
    let (c, e) = run(&size_argv);
    assert_eq!(c, 10, "SIZE must refuse a pending grow job: {e}");
    assert!(e.contains("`resize grow`"), "the refusal must name the job it belongs to: {e}");
    assert!(ckpt.exists(), "a refusal must not touch the scene");

    let (c, e) = run(&grow_argv);
    assert_eq!(c, 0, "re-running the original command must resume and finish: {e}");
    assert!(!ckpt.exists() && !journal.exists(), "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}