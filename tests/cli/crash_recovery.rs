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

    let (c, e) = run_fault("before-entry-commit", &argv);
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

    let (c, e) = run_fault("before-hidden-fix", &argv);
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

    let (c, e) = run_fault("before-entry-commit", &argv);
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

    // 重跑原命令收尾
    let (c, e) = run(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    assert!(!record_present(&img));
}

/// `copy`：数据已复制、表项未提交时崩溃。它**没有续跑路径**（`help copy` 的 "No resume"），
/// 表未被触碰 ⇒ undo 不做释放，需要显式 `abandon`；文案绝不能建议"重跑 copy"
#[test]
fn copy_crash_has_no_resume_path_and_undo_refuses() {
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

    // 表未被触碰、copy 无续跑路径：undo 不把这份现场当"无事可做"释放——`restore_blocked`
    // 就是"写过目标字节"这条事实，静默等价于 `abandon` 会抹掉它的存在。释放需要显式 `abandon`
    let (c, e) = run(&["undo", img_s, "--yes"]);
    assert_eq!(c, 3, "undo must not release a copy record in place of `abandon`: {e}");
    assert!(record_present(&img), "the record must stay until it is explicitly released: {e}");
    let (c, e) = run(&["abandon", img_s, "--yes"]);
    assert_eq!(c, 0, "an explicit `abandon` must release the record: {e}");
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
/// `move` 走单分区路径（chunk 故障点），起点按 MiB 对齐，故位移取对齐后的值
#[test]
fn self_overlap_delta_below_chunk_crash_resume_is_byte_exact() {
    let (img, pattern) = stage_self_overlap("sov");
    let img_s = img.to_str().unwrap();
    // 位移 1MiB（--start 4096，已对齐）< chunk 2MiB：chunk 收敛到 1MiB，共 4 块
    let argv = ["move", &format!("{img_s}:1"), "--start", "4096", "--chunk", "2", "--yes"];
    let (c, e) = run_fault("chunk:2", &argv);
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
    let (c, e) = run_fault("chunk:3", &argv);
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

/// 真实文件系统的收尾步骤要 root、loop 与 e2fsprogs；缺任一项时依赖它的用例跳过
fn fs_tooling_ready() -> bool {
    #[cfg(unix)]
    {
        if unsafe { libc::geteuid() } != 0 {
            return false;
        }
        if !Path::new("/dev/loop-control").exists() {
            return false;
        }
        ["losetup", "mke2fs", "resize2fs", "e2fsck"].iter().all(|name| {
            ["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"]
                .iter()
                .any(|dir| {
                    let path = Path::new(dir).join(name);
                    path.is_file()
                        && path
                            .metadata()
                            .is_ok_and(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0)
                })
        })
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// 带搬移的扩容计划没有围栏载体（围栏只服务无搬移记录）：扩容表已写、进度尚未推进时中断。
/// 重跑必须按盘上的目标区间认出扩容已落、幂等重放该表步。末态按可观测事实对账：两次
/// `resize2fs` 相隔数秒，ext4 的超级块时间戳与日志校验和必然不同，整盘字节不能作为判据
#[test]
fn grow_table_write_crash_converges_to_the_uninterrupted_layout() {
    if !fs_tooling_ready() {
        eprintln!(
            "skipping grow_table_write_crash_converges_to_the_uninterrupted_layout: needs euid 0, \
             /dev/loop-control and losetup/mke2fs/resize2fs/e2fsck"
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("diskedit_gtw_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("g.img");
    let base = dir.join("b.img");
    std::fs::write(&img, vec![0u8; 64 * 1024 * 1024]).unwrap();
    let img_s = img.to_str().unwrap().to_string();
    assert_eq!(run(&["new", &img_s, "--yes"]).0, 0);
    // 目标 8MiB（2048..18431），两条挡路分区各 1MiB（18432..20479、20480..22527）
    assert_eq!(run(&["add", &img_s, "8M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]).0, 0);
    for _ in 0..2 {
        assert_eq!(run(&["add", &img_s, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]).0, 0);
    }
    // 挡路分区填可辨识图案；目标分区建真实 ext4，收尾步骤才有对象可扩
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
    write_region(&img, 18432, &p2);
    write_region(&img, 20480, &p3);
    let target = format!("{img_s}:1");
    let (c, e) = run(&["mkfs", target.as_str(), "ext4", "--yes"]);
    assert_eq!(c, 0, "the fixture must get a real ext4: {e}");

    // 基线副本与中断现场同源：两边只差一次注入
    std::fs::copy(&img, &base).unwrap();
    let base_s = base.to_str().unwrap().to_string();
    let base_target = format!("{base_s}:1");
    let base_argv = ["resize", base_target.as_str(), "grow", "--allow-move", "--chunk", "1", "--yes"];
    assert_eq!(run(&base_argv).0, 0);
    let base_range = gpt_range(&base, 1);

    let argv = ["resize", target.as_str(), "grow", "--allow-move", "--chunk", "1", "--yes"];
    let (c, e) = run_fault("after-grow-table-write", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record_present(&img), "the interrupted job must leave its record: {e}");
    // 中断点必须在扩容表已落之后：目标分区的表项已经等于基线的末态
    assert_eq!(
        gpt_range(&img, 1),
        base_range,
        "the crash must sit after the grow table write and before its record: {e}"
    );

    // 让续跑的 resize2fs 与基线的落进不同秒：末态对账不得依赖时间戳恰好相同
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (c, e) = run(&argv);
    assert_eq!(c, 0, "re-running must converge: {e}");
    assert!(!record_present(&img), "a converged job must commit its record: {e}");

    assert_eq!(
        std::fs::metadata(&img).unwrap().len(),
        std::fs::metadata(&base).unwrap().len(),
        "the capacity must match the uninterrupted baseline"
    );
    assert_eq!(
        info_out(&img),
        info_out(&base),
        "`info` must describe the same layout on both sides"
    );
    assert_eq!(
        table_bytes(&img),
        table_bytes(&base),
        "PMBR, primary header, entry array and backup header must match the baseline"
    );
    for num in 1..=3 {
        assert_eq!(
            gpt_range(&img, num),
            gpt_range(&base, num),
            "partition {num} must land where the baseline puts it"
        );
    }
    // 用户数据：两条被搬分区的数据区原样落在新位置，且与基线同位置逐字节相同
    for (num, want) in [(2usize, &p2), (3usize, &p3)] {
        let (s, t) = gpt_range(&img, num);
        let len = ((t - s + 1) * 512) as usize;
        let bytes = read_region(&img, s, len);
        assert_eq!(bytes, *want, "partition {num} must carry its relocated pattern intact");
        assert_eq!(
            bytes,
            read_region(&base, s, len),
            "partition {num} user data must match the baseline at the same position"
        );
    }
    let (start, end) = gpt_range(&img, 1);
    assert!(
        e2fsck_clean(&img, start, end - start + 1),
        "the resumed filesystem must pass a read-only check"
    );
    assert!(
        e2fsck_clean(&base, start, end - start + 1),
        "the baseline filesystem must pass a read-only check"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `info` 的版面描述（JSON，含容量、扇区、各分区起止与文件系统类型）
fn info_out(img: &Path) -> String {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(["info", img.to_str().unwrap()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// 把分区按 loop 映射出来跑只读 `e2fsck -fn`，退出 0 视为干净
fn e2fsck_clean(img: &Path, start: u64, sectors: u64) -> bool {
    let show = std::process::Command::new("losetup")
        .args([
            "--find",
            "--show",
            "--offset",
            &(start * 512).to_string(),
            "--sizelimit",
            &(sectors * 512).to_string(),
        ])
        .arg(img)
        .output()
        .unwrap();
    if !show.status.success() {
        return false;
    }
    let dev = String::from_utf8_lossy(&show.stdout).trim().to_string();
    let check = std::process::Command::new("e2fsck").args(["-fn", &dev]).output().unwrap();
    let _ = std::process::Command::new("losetup").args(["-d", &dev]).output();
    check.status.success()
}

/// 目标分区右侧紧邻挡路者：`resize grow`（尾部打包）与 `resize SIZE`（最小位移）
/// 各自只有走搬移才能完成，而两条命令共用同一个续跑槽位。按对方的语义执行槽上
/// 那份现场，就是把请求静默放大或缩水——故必须拒绝，且现场原样留给原命令续跑。
///
/// 两个方向都在写表之前被身份判据挡下；各自的原命令重跑则接着槽上的进度做完。
/// 退出码是 5：本次运行未写目标，但记录表明上次作业已越过不可逆点，因此不按 3 报
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

    // 精确 SIZE 作业（最小位移 plan）中断
    // 两个分区都没有 FS：扩分区表要显式 --no-fs（槽位/续跑语义与它无关）
    let size_argv = ["resize", target.as_str(), "+3M", "--allow-move", "--chunk", "1", "--yes", "--no-fs"];
    let (c, e) = run_fault("chunk:1", &size_argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record_present(&img), "a mid-move crash must leave a resumable record: {e}");

    // grow 语义与它不符：拒绝并保持现场
    let grow_argv = ["resize", target.as_str(), "grow", "--allow-move", "--chunk", "1", "--yes", "--no-fs"];
    let (c, e) = run(&grow_argv);
    assert_eq!(c, 5, "grow must refuse a pending SIZE job: {e}");
    assert!(e.contains("`resize SIZE --allow-move`"), "the refusal must name the job it belongs to: {e}");
    assert!(!e.contains("re-run `diskedit resize"), "a mismatched semantic must not be advertised as the way out: {e}");
    assert!(record_present(&img), "a refusal must not touch the scene");

    // 重跑原命令 ⇒ 续跑并收尾
    let (c, e) = run(&size_argv);
    assert_eq!(c, 0, "re-running the original command must resume and finish: {e}");
    assert!(!record_present(&img), "{e}");

    // 反向：grow 作业（尾部打包 plan）中断 → SIZE 语义必须拒绝
    let (c, e) = run_fault("chunk:1", &grow_argv);
    assert_ne!(c, 0, "{e}");
    assert!(record_present(&img), "{e}");
    let (c, e) = run(&size_argv);
    assert_eq!(c, 5, "SIZE must refuse a pending grow job: {e}");
    assert!(e.contains("`resize grow`"), "the refusal must name the job it belongs to: {e}");
    assert!(record_present(&img), "a refusal must not touch the scene");

    // 原命令重跑 ⇒ 接着槽上的进度收尾
    let (c, e) = run(&grow_argv);
    assert_eq!(c, 0, "re-running the original grow command must resume and finish: {e}");
    assert!(!record_present(&img), "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 记录里还没有修复事实：绑定记录之后、修复开始之前崩溃，续跑按本次读到的表重新分类并补做修复。
/// 盘面：镜像被扩到 48MiB，双头自述的备份位置仍停在 32MiB 末端
#[test]
fn resume_after_relocation_bound_replays_the_repair() {
    let img = stage("rbr");
    let img_s = img.to_str().unwrap();
    // 被搬的成员：扩容要动它，记录里才会绑上搬移计划（`Belongs` 续跑路径）
    let (c, e) = run(&["add", img_s, "1M", "--align", "mib", "--type-guid", "0fc63daf-8483-4772-8e79-3d69d8477de4", "--yes"]);
    assert_eq!(c, 0, "the second partition must be added: {e}");
    let grown = 48 * 1024 * 1024u64;
    let f = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
    f.set_len(grown).unwrap();
    drop(f);
    let last = grown / 512 - 1;
    let target = format!("{img_s}:1");
    let argv = ["resize", target.as_str(), "+1M", "--allow-move", "--chunk", "1", "--yes", "--no-fs"];

    let (c, e) = run_fault("after-relocation-bound", &argv);
    assert_ne!(c, 0, "the injected abort must not look like success: {e}");
    assert!(record_present(&img), "the bound record must survive the crash: {e}");
    let stale = read_region(&img, last, 512);
    assert_ne!(
        &stale[0..8],
        b"EFI PART".as_slice(),
        "the crash must land before the repair: no backup header at the new end"
    );

    let (c, out, e) = run_out(&argv);
    assert_eq!(c, 0, "re-running must resume and finish: {e}");
    // 日志行是本次进程真的走过修复步骤的证据；提交本身也会把双头写到容器末端，
    // 只凭末态分不出修复是否执行
    assert!(out.contains("[repair]"), "the resume must run the repair step: {out}\n{e}");
    let primary = read_region(&img, 1, 512);
    let backup = read_region(&img, last, 512);
    assert_eq!(&primary[0..8], b"EFI PART".as_slice());
    assert_eq!(&backup[0..8], b"EFI PART".as_slice(), "the resume must write the backup header at the container end");
    assert_eq!(
        u64::from_le_bytes(primary[32..40].try_into().unwrap()),
        last,
        "the primary must point at the relocated backup"
    );
    assert_eq!(
        u64::from_le_bytes(backup[24..32].try_into().unwrap()),
        last,
        "the backup header must describe itself as the last LBA"
    );
    assert!(!record_present(&img), "{e}");

    std::fs::remove_dir_all(img.parent().unwrap()).ok();
}

/// 表区的可观测量：PMBR、主头、条目数组与容器末端的备份头
fn table_bytes(img: &Path) -> Vec<u8> {
    let sectors = std::fs::metadata(img).unwrap().len() / 512;
    let mut out = read_region(img, 0, 512);
    out.extend(read_region(img, 1, 512));
    out.extend(read_region(img, 2, 1024));
    out.extend(read_region(img, sectors - 1, 512));
    out
}

/// `delete` 的首次落盘就是修复事实：修复的第一个目标字节写出之前，记录已在盘上；
/// 该记录只有修复事实、没有恢复区间，`undo` 仍必须能结算它
#[test]
fn delete_records_the_table_repair_before_its_first_write() {
    let img = stage("drep");
    let img_s = img.to_str().unwrap().to_string();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&img)
        .unwrap()
        .set_len(48 * 1024 * 1024)
        .unwrap();
    let before = table_bytes(&img);
    let target = format!("{img_s}:1");
    let argv = ["delete", target.as_str(), "--yes"];

    let (c, e) = run_fault("repair-recorded", &argv);
    assert_ne!(c, 0, "the fault must abort the run: {e}");
    assert!(
        record_present(&img),
        "the record must be on disk before the repair writes: {e}"
    );
    let shown = inspect_out(&img);
    assert!(shown.contains("repair: "), "inspect must show the repair fact: {shown}");
    assert!(shown.contains("in flight"), "the repair must still be in flight: {shown}");
    assert_eq!(table_bytes(&img), before, "no table byte may be written before the repair record");

    // 记录里只有修复事实、没有可回写的区间：结算不写盘，但记录必须被释放
    let (c, e) = run(&["undo", img_s.as_str(), "--yes"]);
    assert_eq!(c, 0, "undo must settle a record that holds only a repair fact: {e}");
    assert!(!record_present(&img), "{e}");

    let (c, e) = run(&argv);
    assert_eq!(c, 0, "the delete must run again after the record is released: {e}");
    assert!(!record_present(&img), "{e}");

    std::fs::remove_dir_all(img.parent().unwrap()).ok();
}

/// `copy` 同一条路径：记录由修复事实首次落盘，之前没有任何目标字节写出
#[test]
fn copy_records_the_table_repair_before_its_first_write() {
    let img = stage("crep");
    let img_s = img.to_str().unwrap().to_string();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&img)
        .unwrap()
        .set_len(48 * 1024 * 1024)
        .unwrap();
    let before = table_bytes(&img);
    let target = format!("{img_s}:1");
    let argv = [
        "copy",
        target.as_str(),
        "--start",
        "24576",
        "--chunk",
        "1",
        "--yes",
    ];

    let (c, e) = run_fault("repair-recorded", &argv);
    assert_ne!(c, 0, "the fault must abort the run: {e}");
    assert!(
        record_present(&img),
        "the record must be on disk before the repair writes: {e}"
    );
    let shown = inspect_out(&img);
    assert!(shown.contains("repair: "), "inspect must show the repair fact: {shown}");
    assert!(shown.contains("in flight"), "the repair must still be in flight: {shown}");
    assert_eq!(table_bytes(&img), before, "no table byte may be written before the repair record");

    let (c, e) = run(&["undo", img_s.as_str(), "--yes"]);
    assert_eq!(c, 0, "undo must settle a record that holds only a repair fact: {e}");
    assert!(!record_present(&img), "{e}");

    let (c, e) = run(&argv);
    assert_eq!(c, 0, "the copy must run again after the record is released: {e}");
    assert!(!record_present(&img), "{e}");

    std::fs::remove_dir_all(img.parent().unwrap()).ok();
}

/// `delete` 的表步围栏先于表写入：围栏在途时表字节未动，阶段判不可判定，
/// 记录没有可回写的区间，只能用 `abandon` 释放
#[test]
fn delete_records_the_table_step_before_its_first_write() {
    let img = stage("dfence");
    let img_s = img.to_str().unwrap().to_string();
    let before = table_bytes(&img);
    let target = format!("{img_s}:1");
    let argv = ["delete", target.as_str(), "--yes"];

    let (c, e) = run_fault("table-step-recorded", &argv);
    assert_ne!(c, 0, "the fault must abort the run: {e}");
    assert!(
        record_present(&img),
        "the record must be on disk before the table writes: {e}"
    );
    let shown = inspect_out(&img);
    assert!(
        shown.contains("table fence: table step in flight"),
        "inspect must show the in-flight fence: {shown}"
    );
    assert!(
        shown.contains("stage: Undecidable"),
        "an in-flight fence must leave the stage undecidable: {shown}"
    );
    assert_eq!(table_bytes(&img), before, "no table byte may be written before the fence");

    assert_eq!(run(&["abandon", img_s.as_str(), "--yes"]).0, 0);
    assert!(!record_present(&img));

    let (c, e) = run(&argv);
    assert_eq!(c, 0, "the delete must run again after the record is released: {e}");
    assert!(!record_present(&img), "{e}");

    std::fs::remove_dir_all(img.parent().unwrap()).ok();
}

/// 表步写完、记录尚未提交：围栏已经 `Landed`，阶段说明表已写，`undo` 按恢复区间回放
#[test]
fn delete_records_the_landed_table_step() {
    let img = stage("dfence2");
    let img_s = img.to_str().unwrap().to_string();
    let before = table_bytes(&img);
    let target = format!("{img_s}:1");
    let argv = ["delete", target.as_str(), "--yes"];

    let (c, e) = run_fault("after-table-step", &argv);
    assert_ne!(c, 0, "the fault must abort the run: {e}");
    assert!(record_present(&img), "the record must survive the crash: {e}");
    let shown = inspect_out(&img);
    assert!(
        shown.contains("table fence: landed"),
        "inspect must show the landed fence: {shown}"
    );
    assert!(
        shown.contains("stage: TableWritten"),
        "a landed fence must report the table as written: {shown}"
    );

    let (c, e) = run(&["undo", img_s.as_str(), "--yes"]);
    assert_eq!(c, 0, "undo must replay the recorded table preimages: {e}");
    assert!(!record_present(&img), "{e}");
    assert_eq!(table_bytes(&img), before, "undo must restore the table bytes");

    std::fs::remove_dir_all(img.parent().unwrap()).ok();
}
