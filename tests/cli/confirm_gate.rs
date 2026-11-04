//! 确认门：几何推导出的破坏面决定是否需要 --yes，而不是命令名。
//! 每个命令两条路径——缺 --yes 拒绝（10）且一处不改盘；带 --yes 越过门继续执行。

use std::path::{Path, PathBuf};
use std::process::Command;

const LOCK_SUFFIX: &str = ".diskedit.lock";
const JOURNAL_SUFFIX: &str = ".diskedit.journal";
const CKPT_SUFFIX: &str = ".diskedit.ckpt";

fn run(args: &[&str]) -> (i32, String, String) {
    let exe = env!("CARGO_BIN_EXE_DiskEdit");
    let out = Command::new(exe).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn suffix(img: &Path, tail: &str) -> PathBuf {
    let mut os = img.as_os_str().to_owned();
    os.push(tail);
    PathBuf::from(os)
}

fn read_optional(p: &Path) -> Option<Vec<u8>> {
    std::fs::read(p).ok()
}

/// 一次涉及写盘的操作在拒绝前后必须逐字节可比的现场。
#[derive(Debug, PartialEq, Eq)]
struct Scene {
    image: Vec<u8>,
    journal: Option<Vec<u8>>,
    ckpt: Option<Vec<u8>>,
    lock: Option<Vec<u8>>,
}

fn scene(img: &Path) -> Scene {
    Scene {
        image: std::fs::read(img).unwrap(),
        journal: read_optional(&suffix(img, JOURNAL_SUFFIX)),
        ckpt: read_optional(&suffix(img, CKPT_SUFFIX)),
        lock: read_optional(&suffix(img, LOCK_SUFFIX)),
    }
}

fn fresh(tag: &str, bytes: usize) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("diskedit_cfg_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("c.img");
    std::fs::write(&img, vec![0u8; bytes]).unwrap();
    let (c, _, e) = run(&["new", img.to_str().unwrap(), "--yes"]);
    assert_eq!(c, 0, "fixture needs a GPT target: {e}");
    (dir, img)
}

/// 拒绝文案同时点名被破坏的对象与不可回滚性。
fn assert_refusal(code: i32, stderr: &str) {
    assert_eq!(code, 10, "a missing --yes must refuse with 10: {stderr}");
    assert!(stderr.contains("cannot be rolled back with `undo`"), "{stderr}");
    assert!(stderr.contains("--yes"), "{stderr}");
}

/// 在拒绝路径上钉住"一处不改盘"：镜像、journal、checkpoint、锁文件全部原样。
fn assert_untouched(before: &Scene, after: &Scene, what: &str) {
    assert_eq!(before, after, "{what} refused without --yes must leave every byte and sidecar unchanged");
}

fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).map(str::to_string))
        .and_then(|l| l.split_whitespace().nth(2).map(|f| f == "0"))
        .unwrap_or(false)
}

#[test]
fn move_asks_when_the_partition_data_moves() {
    let (dir, img) = fresh("move", 8 * 1024 * 1024);
    let img_s = img.to_str().unwrap();
    assert_eq!(run(&["add", img_s, "--start", "2048", "--end", "4095"]).0, 0);

    let before = scene(&img);
    let (c, _, e) = run(&["move", &format!("{img_s}:1"), "--start", "8192"]);
    assert_refusal(c, &e);
    assert_untouched(&before, &scene(&img), "move");

    let (c, _, e) = run(&["move", &format!("{img_s}:1"), "--start", "8192", "--yes"]);
    assert_eq!(c, 0, "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn copy_always_asks() {
    let (dir, img) = fresh("copy", 8 * 1024 * 1024);
    let img_s = img.to_str().unwrap();
    assert_eq!(run(&["add", img_s, "--start", "2048", "--end", "4095"]).0, 0);

    let before = scene(&img);
    let (c, _, e) = run(&["copy", &format!("{img_s}:1"), "--start", "6144"]);
    assert_refusal(c, &e);
    assert_untouched(&before, &scene(&img), "copy");

    let (c, _, e) = run(&["copy", &format!("{img_s}:1"), "--start", "6144", "--yes"]);
    assert_eq!(c, 0, "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// resize-part 的确认门由盘上几何推导：起点变化（搬移）或末端下移（缩容）都要
/// --yes；纯扩容与"请求范围即现状"都不问。
#[test]
fn resize_part_asks_for_relocation_and_shrink() {
    let (dir, img) = fresh("resizepart", 16 * 1024 * 1024);
    let img_s = img.to_str().unwrap();
    // 真实可缩的 ext4：只有它能把"门拒"与"FS 缩容守卫拒"区分开——后者在旧实现
    // 下同样给 10，用它做夹具证明拒绝来自门而不是另一条守卫
    let (c, _, e) = run(&["create", img_s, "--size", "8M", "--fs", "ext4", "--yes"]);
    if c != 0 {
        assert!(!is_root(), "root-only fixture failed unexpectedly: {e}");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let p1 = format!("{img_s}:1");

    // 缩小且位移：起点 2048→4096、末端 18431→16383
    let before = scene(&img);
    let (c, _, e) = run(&["resize-part", &p1, "--start", "4096", "--end", "16383"]);
    assert_refusal(c, &e);
    assert_untouched(&before, &scene(&img), "resize-part shrink with relocation");

    // 缩小但不位移：起点不动、末端下移——旧实现不经确认门就直接缩 FS 写表
    let before = scene(&img);
    let (c, _, e) = run(&["resize-part", &p1, "--start", "2048", "--end", "16383"]);
    assert_refusal(c, &e);
    assert_untouched(&before, &scene(&img), "resize-part shrink without relocation");

    // --yes 放行：证明该请求本身可执行，10 来自门而非别的守卫
    let (c, _, e) = run(&["resize-part", &p1, "--start", "2048", "--end", "16383", "--yes"]);
    assert_eq!(c, 0, "a shrink with --yes must proceed: {e}");
    let gpt = gptman::GPT::find_from(&mut std::fs::File::open(&img).unwrap()).unwrap();
    assert_eq!(gpt[1].ending_lba, 16383, "the shrink must land on the requested end");

    // 纯扩容：末端回到右侧空闲区，不搬数据也不缩——不需要 --yes
    let (c, _, e) = run(&["resize-part", &p1, "--start", "2048", "--end", "18431", "--no-fs"]);
    assert_eq!(c, 0, "a pure grow must not ask for --yes: {e}");

    // 无操作：请求范围与现状相同——不需要 --yes
    let (c, _, e) = run(&["resize-part", &p1, "--start", "2048", "--end", "18431", "--no-fs"]);
    assert_eq!(c, 0, "a no-op resize must not ask for --yes: {e}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_asks_when_a_blocker_moves_and_not_otherwise() {
    let (dir, img) = fresh("apply", 8 * 1024 * 1024);
    let img_s = img.to_str().unwrap();
    assert_eq!(run(&["add", img_s, "--start", "2048", "--end", "4095"]).0, 0);
    assert_eq!(run(&["add", img_s, "--start", "4096", "--end", "6143"]).0, 0);

    let before = scene(&img);
    let (c, _, e) = run(&["apply", img_s, "--grow", "1", "--no-fs"]);
    assert_refusal(c, &e);
    assert_untouched(&before, &scene(&img), "apply relocation");

    let (c, _, e) = run(&["apply", img_s, "--grow", "1", "--no-fs", "--yes"]);
    assert_eq!(c, 0, "{e}");

    // 对照：目标右侧就是空闲区，扩容只改表——不需要 --yes
    let (dir2, img2) = fresh("applyfree", 8 * 1024 * 1024);
    let img2_s = img2.to_str().unwrap();
    assert_eq!(run(&["add", img2_s, "--start", "2048", "--end", "4095"]).0, 0);
    let (c, _, e) = run(&["apply", img2_s, "--grow", "1", "--no-fs"]);
    assert_eq!(c, 0, "growing into free space must not ask for --yes: {e}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

#[test]
fn resize_asks_when_the_target_shrinks() {
    let (dir, img) = fresh("resize", 16 * 1024 * 1024);
    let img_s = img.to_str().unwrap();
    // 缩容要过 FS 守卫链：先建一个可缩的 ext4（需要 loop/mkfs，非 root 环境跳过）
    let (c, _, e) = run(&["create", img_s, "--size", "8M", "--fs", "ext4", "--yes"]);
    if c != 0 {
        assert!(!is_root(), "root-only fixture failed unexpectedly: {e}");
        return;
    }

    let before = scene(&img);
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "6M"]);
    assert_refusal(c, &e);
    assert_untouched(&before, &scene(&img), "resize shrink");

    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "6M", "--yes"]);
    assert_eq!(c, 0, "a shrink with --yes must proceed: {e}");

    // 对照：纯扩容不缩，不需要 --yes
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "9M", "--no-fs"]);
    assert_eq!(c, 0, "a grow must not ask for --yes: {e}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn create_with_fs_asks_before_writing_the_table() {
    let (dir, img) = fresh("create", 8 * 1024 * 1024);
    let img_s = img.to_str().unwrap();

    let before = scene(&img);
    let (c, _, e) = run(&["create", img_s, "--size", "4M", "--fs", "ext4"]);
    assert_refusal(c, &e);
    assert_untouched(&before, &scene(&img), "create --fs");

    let (c, _, e) = run(&["create", img_s, "--size", "4M", "--fs", "ext4", "--yes"]);
    if is_root() {
        assert_eq!(c, 0, "{e}");
    } else {
        assert_ne!(c, 10, "with --yes the gate must be open: {e}");
    }

    // 对照：不带 --fs 只建条目，可由 undo 撤销，不需要 --yes（独立镜像，避免占用同一空闲区）
    let (dir2, img2) = fresh("createbare", 8 * 1024 * 1024);
    let (c, _, e) = run(&["create", img2.to_str().unwrap(), "--size", "4M"]);
    assert_eq!(c, 0, "creating a bare entry must not ask for --yes: {e}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}
