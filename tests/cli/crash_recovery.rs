use std::path::Path;

fn run(args: &[&str]) -> (i32, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit")).args(args).output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// `inspect` 的可观察输出：记录在不在、进度停在哪、哪条出路都在里面。
/// 恢复现场按它比对，不按记录目录内的文件名取字节
fn inspect_out(img: &Path) -> String {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(["inspect", img.to_str().unwrap()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// 目标上有可识别的恢复记录
fn record_present(img: &Path) -> bool {
    inspect_out(img).lines().any(|l| l == "record: recognized")
}

/// 搬移进度里已 durable 的游标；没有记录、或记录里没有进度行时为 `None`
fn record_cursor(img: &Path) -> Option<u64> {
    inspect_out(img).lines().find_map(|l| {
        let rest = l.trim_start().strip_prefix("progress: ")?;
        rest.split_whitespace()
            .find_map(|f| f.strip_prefix("cursor="))
            .and_then(|v| v.parse().ok())
    })
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

/// 与 `run` 同一命令，连同 stdout 一起返回：续跑日志（游标位置）只走 stdout
fn run_out(args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit")).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// MBR 第 1 个表项的终点 LBA（读 LBA0 的 446 处条目）
fn mbr_first_end(img: &Path) -> u64 {
    use std::io::Read;
    let mut b = [0u8; 512];
    std::fs::File::open(img).unwrap().read_exact(&mut b).unwrap();
    let start = u32::from_le_bytes(b[446 + 8..446 + 12].try_into().unwrap());
    let len = u32::from_le_bytes(b[446 + 12..446 + 16].try_into().unwrap());
    u64::from(start) + u64::from(len) - 1
}

/// GPT 第 `num` 号分区的 (start, end)：表项数组的位置取自 LBA1 的表头
fn gpt_range(img: &Path, num: usize) -> (u64, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let mut hdr = [0u8; 512];
    let mut f = std::fs::File::open(img).unwrap();
    f.seek(SeekFrom::Start(512)).unwrap();
    f.read_exact(&mut hdr).unwrap();
    let entries_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
    let mut e = [0u8; 128];
    f.seek(SeekFrom::Start(entries_lba * 512 + (num as u64 - 1) * 128)).unwrap();
    f.read_exact(&mut e).unwrap();
    (
        u64::from_le_bytes(e[32..40].try_into().unwrap()),
        u64::from_le_bytes(e[40..48].try_into().unwrap()),
    )
}

fn read_region(img: &Path, lba: u64, len: usize) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(img).unwrap();
    f.seek(SeekFrom::Start(lba * 512)).unwrap();
    let mut v = vec![0u8; len];
    f.read_exact(&mut v).unwrap();
    v
}

fn write_region(img: &Path, lba: u64, bytes: &[u8]) {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(img).unwrap();
    f.seek(SeekFrom::Start(lba * 512)).unwrap();
    f.write_all(bytes).unwrap();
}

/// 32MiB 镜像 + 一个 2048..4095 的分区
fn stage(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("diskedit_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("c.img");
    std::fs::write(&img, vec![0u8; 32 * 1024 * 1024]).unwrap();
    let img_s = img.to_str().unwrap();
    assert_eq!(run(&["new", img_s, "--yes"]).0, 0);
    assert_eq!(run(&["add", img_s, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]).0, 0);
    img
}

/// `move`：数据已搬、表项未提交时崩溃。记录里留着 durable 进度 ⇒ 出路是**重跑原命令续跑**
#[test]
fn resize_part_crash_leaves_a_resumable_transaction() {
    let img = stage("rsc");
    let img_s = img.to_str().unwrap();
    let target = format!("{img_s}:1");
    let argv = ["move", target.as_str(), "--start", "8192", "--chunk", "4", "--yes"];

    let (c, e) = run_fault("rs-before-commit", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record_present(&img), "the transaction's record must survive the crash: {e}");
    assert!(
        record_cursor(&img).unwrap_or(0) > 0,
        "a mid-move crash must leave durable transfer progress: {e}"
    );
    let record_before = inspect_out(&img);

    // 另一条新操作不得接手，且不得动那份记录
    let (c, e) = run(&["add", img_s, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]);
    assert_eq!(c, 5, "another mutator must be refused: {e}");
    assert!(e.contains("owns this target"), "{e}");
    assert!(e.contains("re-run `diskedit move` with the same request"), "with durable progress the way out is resume: {e}");
    assert_eq!(inspect_out(&img), record_before, "a refusal must not touch the record");

    // 重跑原命令 ⇒ 续跑并收尾
    let (c, e) = run(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    assert!(!record_present(&img), "a finished transaction must be committed: {e}");
}

/// `move` 搬移 NTFS 分区：表项提交后、HiddenSectors 修正前崩溃。
/// 已提交态续跑时盘上几何已是新值，修正与否必须由记录里的搬移前起点判定
#[test]
fn ntfs_hidden_sectors_fixed_when_resuming_after_commit() {
    use std::io::{Read, Seek, Write};
    let img = stage("nhs");
    let img_s = img.to_str().unwrap();
    let target = format!("{img_s}:1");
    let argv = ["move", target.as_str(), "--start", "8192", "--chunk", "4", "--yes"];

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
    assert!(record_present(&img), "a committed-but-unfinished crash leaves the record: {e}");

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
    assert!(!record_present(&img), "a finished transaction must be cleaned up");
}

/// copy 的严格打开：搬移类作业的中途现场对 `resize` / `copy` 都不可续跑
/// ——`resize` 对它拒绝且出路文案不得误导（重跑 resize 救不了它）；`copy` 没有续跑
/// 能力，绝不静默接管别人的现场（否则成功后会把人家的记录当自己的清掉）
#[test]
fn foreign_resize_slot_refuses_resize_and_copy() {
    let img = stage("frs");
    let img_s = img.to_str().unwrap();
    let target = format!("{img_s}:1");
    let argv = ["move", target.as_str(), "--start", "8192", "--chunk", "4", "--yes"];

    let (c, e) = run_fault("rs-before-commit", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record_present(&img), "a mid-move crash must leave a resumable record: {e}");
    let record_before = inspect_out(&img);

    // resize：拒绝（记录尚未提交 ⇒ 落 5），且出路文案给出真正能续跑的命令
    let (c, e) = run(&["resize", target.as_str(), "grow", "--allow-move", "--chunk", "4", "--yes"]);
    assert_eq!(c, 5, "resize must refuse a foreign single-partition job: {e}");
    assert!(
        e.contains("re-run `diskedit move` with the same request") && e.contains("abandon"),
        "the way out must name a resuming command and the release: {e}"
    );
    // copy：同样拒绝，不得接管
    let (c, e) = run(&["copy", target.as_str(), "--start", "end", "--chunk", "4", "--yes"]);
    assert_eq!(c, 5, "copy must refuse while any job owns the slot: {e}");
    // 两次拒绝都不得动现场
    assert_eq!(inspect_out(&img), record_before, "a refusal must not touch the record");

    // 重跑原命令收尾（对账：现场本身仍是可续跑的）
    let (c, e) = run(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    assert!(!record_present(&img));
}

/// `copy`：数据已复制、表项未提交时崩溃。它**没有续跑路径**（`help copy` 的 "No resume"），
/// 表区 preimage 仍记在记录里 ⇒ 出路是 undo 回滚或 abandon 释放；文案绝不能建议"重跑 copy"
#[test]
fn copy_crash_has_no_resume_path_and_undo_rolls_the_table_back() {
    let img = stage("cpc");
    let img_s = img.to_str().unwrap();
    let target = format!("{img_s}:1");

    let (c, e) = run_fault("copy-before-commit", &["copy", target.as_str(), "--start", "20480", "--chunk", "1", "--yes"]);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record_present(&img), "the record must survive the crash: {e}");

    let (c, e) = run(&["add", img_s, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]);
    assert_eq!(c, 5, "another mutator must be refused: {e}");
    assert!(e.contains("owns this target"), "{e}");
    assert!(e.contains("no resume path"), "copy offers no resume: {e}");
    assert!(
        !e.contains("re-run `diskedit copy`"),
        "copy must never be advertised as resumable: {e}"
    );

    // 被推荐的那条出路必须真的走得通：undo 回滚表区并清理记录
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 0, "the recorded table pre-images must roll back: {e}");
    assert!(!record_present(&img), "{e}");
    let (c, e) = run(&["add", img_s, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]);
    assert_eq!(c, 0, "the target must be usable again: {e}");
}

/// MBR 的扩容由与 GPT 同一族记录承载：表项已写、FS 步尚未做时中断，重跑从记录里的
/// 提交前区间认出"分区层已提交"，直接收尾；按现算的右侧空闲分流会把它拒成"右侧无空闲"。
/// 分区数据在中断与续跑前后不变
#[test]
fn mbr_grow_crash_is_resumed_by_re_running() {
    let dir = std::env::temp_dir().join(format!("diskedit_mbr_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("m.img");
    std::fs::write(&img, vec![0u8; 32 * 1024 * 1024]).unwrap();
    let img_s = img.to_str().unwrap();
    assert_eq!(run(&["new", img_s, "--yes", "--table", "mbr"]).0, 0);
    assert_eq!(run(&["add", img_s, "1M", "--align", "mib", "--yes"]).0, 0);
    // 分区数据：扩表与续跑都不得改它一个字节
    let mut pattern = vec![0u8; 1024 * 1024];
    let mut x = 0x2468u32;
    for b in pattern.iter_mut() {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        *b = (x >> 24) as u8;
    }
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
        f.seek(SeekFrom::Start(2048 * 512)).unwrap();
        f.write_all(&pattern).unwrap();
    }
    let target = format!("{img_s}:1");
    let argv = ["resize", target.as_str(), "grow", "--no-fs", "--yes"];

    let before = mbr_first_end(&img);
    assert_eq!(before, 4095);
    let (c, e) = run_fault("mbr-after-table-write", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record_present(&img), "the table write is recorded: {e}");
    let committed = mbr_first_end(&img);
    assert!(committed > before, "the interrupted run must have committed the grown entry: {e}");

    // 续跑：不再按现算的右侧空闲拒绝，只收尾并清掉记录
    let (c, _out, e) = run_out(&argv);
    assert_eq!(c, 0, "re-running must resume the recorded job: {e}");
    assert!(
        !e.contains("no free space to its right"),
        "the resume must not fall back to the unused-space check: {e}"
    );
    assert!(!record_present(&img), "a finished job must be committed: {e}");
    assert_eq!(mbr_first_end(&img), committed, "the resume must not grow the table a second time");
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(&raw[2048 * 512..(2048 + 2048) * 512], &pattern[..], "the partition data must survive byte for byte");
    let _ = std::fs::remove_dir_all(&dir);
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
    assert_eq!(run(&["add", img_s, "4M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]).0, 0);
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
    // 位移 1MiB（--start 4096，已对齐）< chunk 2MiB：chunk 收敛到 1MiB，共 4 块
    let argv = ["move", &format!("{img_s}:1"), "--start", "4096", "--chunk", "2", "--yes"];
    let (c, e) = run_fault("rs-chunk:2", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(
        record_cursor(&img).unwrap_or(0) > 0,
        "a mid-move crash must leave durable transfer progress: {e}"
    );

    let (c, e) = run(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    assert!(!record_present(&img), "{e}");
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(&raw[4096 * 512..(4096 + 8192) * 512], &pattern[..], "relocated bytes must match the original pattern");
}

/// 自重叠搬移的另一组尺寸（位移 2MiB > chunk 1MiB）：chunk 不收敛，但自重叠仍逐 chunk 推进
/// durable 边界——重做只从最后一个已落盘 chunk 之后开始
#[test]
fn self_overlap_delta_above_chunk_crash_resume_is_byte_exact() {
    let (img, pattern) = stage_self_overlap("sov2");
    let img_s = img.to_str().unwrap();
    // 位移 2MiB（--start 6144，已对齐）> chunk 1MiB，共 4 块
    let argv = ["move", &format!("{img_s}:1"), "--start", "6144", "--chunk", "1", "--yes"];
    let (c, e) = run_fault("rs-chunk:3", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(
        record_cursor(&img).unwrap_or(0) > 0,
        "a mid-move crash must leave durable transfer progress: {e}"
    );

    let (c, e) = run(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    assert!(!record_present(&img), "{e}");
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
    assert_eq!(run(&["add", img_s, "4M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]).0, 0);
    assert_eq!(run(&["add", img_s, "24M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]).0, 0);
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
    let argv = ["resize", &format!("{img_s}:1"), "+1M", "--allow-move", "--chunk", "1", "--yes", "--no-fs"];
    let (c, e) = run_fault("chunk:17", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record_present(&img), "the interrupted job must leave its record: {e}");

    let (c, e) = run(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    assert!(!record_present(&img), "{e}");
    let raw = std::fs::read(&img).unwrap();
    assert_eq!(&raw[(10240 + 2048) * 512..(10240 + 2048 + 49152) * 512], &pattern[..], "relocated bytes must match the original pattern");
}

/// 多分区联动（尾部打包）：两个挡路分区依次搬移，第一个条目提交、游标推进后中断。
/// 重跑必须从 durable 游标（entry 1）继续——把已搬走条目的旧源区间改脏，若重跑退回
/// entry 0 重新拷贝，它的目的地就会变成脏字节，逐字节对账钉住这一点
#[test]
fn tail_packed_multi_partition_crash_resumes_from_the_durable_cursor() {
    let dir = std::env::temp_dir().join(format!("diskedit_tpm_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("t.img");
    std::fs::write(&img, vec![0u8; 64 * 1024 * 1024]).unwrap();
    let img_s = img.to_str().unwrap();
    assert_eq!(run(&["new", img_s, "--yes"]).0, 0);
    for _ in 0..3 {
        assert_eq!(run(&["add", img_s, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]).0, 0);
    }
    // 两条挡路分区各 1MiB（4096..6143、6144..8191），填可辨识图案用于搬移后对账
    let pattern = |seed: u32| {
        let mut v = vec![0u8; 1024 * 1024];
        let mut x = seed;
        for b in v.iter_mut() {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (x >> 24) as u8;
        }
        v
    };
    let (p2, p3) = (pattern(0x1111), pattern(0x2222));
    write_region(&img, 4096, &p2);
    write_region(&img, 6144, &p3);

    let target = format!("{img_s}:1");
    let argv = ["resize", target.as_str(), "grow", "--allow-move", "--chunk", "1", "--yes", "--no-fs"];
    let (c, e) = run_fault("after-entry-commit:0", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record_present(&img), "the interrupted job must leave its record: {e}");

    // 中断现场：第一个被搬的条目已提交（哪一个先搬由计划决定，按盘上位置判断）
    let moved_lba = if gpt_range(&img, 3).0 != 6144 {
        6144u64
    } else if gpt_range(&img, 2).0 != 4096 {
        4096u64
    } else {
        panic!("after the first entry commit one blocker must have moved: {e}");
    };
    // 把它的旧源区间改脏：游标被尊重时这些字节不会再被读
    write_region(&img, moved_lba, &vec![0x5Au8; 1024 * 1024]);

    let (c, out, e) = run_out(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    assert!(
        out.contains("resuming at entry 1 chunk 0 (durable record)"),
        "the re-run must continue at the durable cursor, not restart at entry 0: {out}"
    );
    assert!(!record_present(&img), "{e}");

    for (num, want) in [(2usize, &p2), (3usize, &p3)] {
        let (s, t) = gpt_range(&img, num);
        assert_eq!(
            read_region(&img, s, ((t - s + 1) * 512) as usize),
            *want,
            "partition {num} must land byte for byte at its new position"
        );
    }
    assert_eq!(
        read_region(&img, moved_lba, 1024 * 1024),
        vec![0x5Au8; 1024 * 1024],
        "a resumed run must not re-read the already-moved source range"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 目标分区右侧紧邻挡路者：`resize grow`（尾部打包）与 `resize SIZE`（最小位移）
/// 各自只有走搬移才能完成，而两条命令共用同一个续跑槽位。按对方的语义执行槽上
/// 那份现场，就是把请求静默放大或缩水——故必须拒绝，且现场原样留给原命令续跑。
///
/// 两个方向都在写表之前被身份判据挡下；各自的原命令重跑则接着槽上的进度做完
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
    assert_eq!(run(&["add", img_s, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]).0, 0);
    assert_eq!(run(&["add", img_s, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]).0, 0);

    // ① 精确 SIZE 作业（最小位移 plan）中断
    // 两个分区都没有 FS：扩分区表要显式 --no-fs（槽位/续跑语义与它无关）
    let size_argv = ["resize", target.as_str(), "+3M", "--allow-move", "--chunk", "1", "--yes", "--no-fs"];
    let (c, e) = run_fault("chunk:1", &size_argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record_present(&img), "a mid-move crash must leave a resumable record: {e}");

    // grow 语义与它不符：拒绝并保持现场
    let grow_argv = ["resize", target.as_str(), "grow", "--allow-move", "--chunk", "1", "--yes", "--no-fs"];
    let (c, e) = run(&grow_argv);
    assert_eq!(c, 3, "grow must refuse a pending SIZE job: {e}");
    assert!(e.contains("`resize SIZE --allow-move`"), "the refusal must name the job it belongs to: {e}");
    assert!(!e.contains("re-run `diskedit resize"), "a mismatched semantic must not be advertised as the way out: {e}");
    assert!(record_present(&img), "a refusal must not touch the scene");

    // 重跑原命令 ⇒ 续跑并收尾
    let (c, e) = run(&size_argv);
    assert_eq!(c, 0, "re-running the original command must resume and finish: {e}");
    assert!(!record_present(&img), "{e}");

    // ② 反向：grow 作业（尾部打包 plan）中断 → SIZE 语义必须拒绝
    let (c, e) = run_fault("chunk:1", &grow_argv);
    assert_ne!(c, 0, "{e}");
    assert!(record_present(&img), "{e}");
    let (c, e) = run(&size_argv);
    assert_eq!(c, 3, "SIZE must refuse a pending grow job: {e}");
    assert!(e.contains("`resize grow`"), "the refusal must name the job it belongs to: {e}");
    assert!(record_present(&img), "a refusal must not touch the scene");

    // 原命令重跑 ⇒ 接着槽上的进度收尾
    let (c, e) = run(&grow_argv);
    assert_eq!(c, 0, "re-running the original grow command must resume and finish: {e}");
    assert!(!record_present(&img), "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}