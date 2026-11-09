//! 确认门：`Confirmation::Required` 的命令缺 `--yes` 在解析期落 2。
//! 每条命令两条路径：缺 --yes 拒绝且一处不改盘；带 --yes 越过门继续执行。

use std::path::{Path, PathBuf};
use std::process::Command;

const RECORD_DIR_SUFFIX: &str = ".diskedit";
const RECORD_LOCK_NAME: &str = "record.lock";
const LINUX_DATA: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";

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
///
/// 恢复现场经 `inspect` 的可观察输出比对（记录在不在、进度停在哪、哪条出路），
/// 不按记录目录内的文件名取字节：落点是实现的内部布局，判据是命令自己能报告的状态。
#[derive(Debug, PartialEq, Eq)]
struct Scene {
    image: Vec<u8>,
    record: (i32, String),
    lock: Option<Vec<u8>>,
}

fn scene(img: &Path) -> Scene {
    let (c, o, e) = run(&["inspect", img.to_str().unwrap()]);
    Scene {
        image: std::fs::read(img).unwrap(),
        record: (c, format!("{o}{e}")),
        lock: read_optional(&suffix(img, RECORD_DIR_SUFFIX).join(RECORD_LOCK_NAME)),
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

/// `add` 取绝对 SIZE；GPT 必须显式给类型 GUID。
fn add_one(img: &str) {
    let (c, _, e) = run(&["add", img, "1M", "--align", "mib", "--type-guid", LINUX_DATA, "--yes"]);
    assert_eq!(c, 0, "fixture needs one partition: {e}");
}

/// 缺 --yes 在解析期落 2，错误文案点名该旗标。
fn assert_refusal(code: i32, stderr: &str) {
    assert_eq!(code, 2, "a missing --yes must fail at parse time with 2: {stderr}");
    assert!(stderr.contains("--yes"), "{stderr}");
}

/// 在拒绝路径上钉住"一处不改盘"：镜像、记录状态、锁文件全部原样。
fn assert_untouched(before: &Scene, after: &Scene, what: &str) {
    assert_eq!(before, after, "{what} refused without --yes must leave every byte and the record state unchanged");
}

#[test]
fn move_requires_yes_at_parse_time() {
    let (dir, img) = fresh("move", 8 * 1024 * 1024);
    let img_s = img.to_str().unwrap();
    add_one(img_s);

    let before = scene(&img);
    let (c, _, e) = run(&["move", &format!("{img_s}:1"), "--start", "8192", "--chunk", "4"]);
    assert_refusal(c, &e);
    assert_untouched(&before, &scene(&img), "move");

    let (c, _, e) = run(&["move", &format!("{img_s}:1"), "--start", "8192", "--chunk", "4", "--yes"]);
    assert_eq!(c, 0, "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn copy_requires_yes_at_parse_time() {
    let (dir, img) = fresh("copy", 8 * 1024 * 1024);
    let img_s = img.to_str().unwrap();
    add_one(img_s);

    let before = scene(&img);
    let (c, _, e) = run(&["copy", &format!("{img_s}:1"), "--start", "6144", "--chunk", "4"]);
    assert_refusal(c, &e);
    assert_untouched(&before, &scene(&img), "copy");

    let (c, _, e) = run(&["copy", &format!("{img_s}:1"), "--start", "6144", "--chunk", "4", "--yes"]);
    assert_eq!(c, 0, "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resize_part_requires_yes() {
    // 夹具经 losetup 真建 ext4，需要 root；无权限的环境跳过
    // SAFETY: geteuid 无参数、不访问内存，恒成功
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skip: root required to build a real filesystem fixture");
        return;
    }
    let (dir, img) = fresh("resizepart", 16 * 1024 * 1024);
    let img_s = img.to_str().unwrap();
    let (c, _, e) = run(&["add", img_s, "8M", "--align", "mib", "--type-guid", LINUX_DATA, "--yes"]);
    assert_eq!(c, 0, "fixture needs one partition: {e}");
    let p1 = format!("{img_s}:1");
    // 缩容守卫要认出 FS：未知类型一律拒绝，与确认门无关
    let (c, _, e) = run(&["mkfs", &p1, "ext4", "--yes"]);
    assert_eq!(c, 0, "fixture needs a recognizable filesystem: {e}");

    let before = scene(&img);
    let (c, _, e) = run(&["resize-part", &p1, "--end", "16383"]);
    assert_refusal(c, &e);
    assert_untouched(&before, &scene(&img), "resize-part");

    // --yes 放行：证明该请求本身可执行，2 来自解析期而非别的守卫
    let (c, _, e) = run(&["resize-part", &p1, "--end", "16383", "--yes"]);
    assert_eq!(c, 0, "a shrink with --yes must proceed: {e}");
    let gpt = gptman::GPT::find_from(&mut std::fs::File::open(&img).unwrap()).unwrap();
    assert_eq!(gpt[1].ending_lba, 16383, "the shrink must land on the requested end");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resize_requires_yes() {
    let (dir, img) = fresh("resize", 16 * 1024 * 1024);
    let img_s = img.to_str().unwrap();
    add_one(img_s);

    let before = scene(&img);
    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "+1M", "--no-fs"]);
    assert_refusal(c, &e);
    assert_untouched(&before, &scene(&img), "resize");

    let (c, _, e) = run(&["resize", &format!("{img_s}:1"), "+1M", "--no-fs", "--yes"]);
    assert_eq!(c, 0, "{e}");

    let _ = std::fs::remove_dir_all(&dir);
}
