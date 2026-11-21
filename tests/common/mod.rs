//! VM 载体用例共用的环境闸门与进程工具。
//!
//! 三个用例文件都以 `mod common;` 引入本目录（`tests/common/` 是目录，不是独立 target）。
//! 它们要回答的是同一件事——"这台机器够不够格跑载体用例"——所以答案只写一份：
//! 缺环境时打印补验命令后跳过；`DISKEDIT_REQUIRE_CAPABILITIES` 置位时跳过即失败。
//!
//! `dead_code` 放行的理由：每个测试 crate 只用其中一部分，某个入口在本 crate 里没有
//! 调用点，却在另一个 crate 里是主路径。

#![allow(dead_code)]
// 跳过说明直接写 stderr，写失败无处上报：这条路径本身就在报告"这条断言没跑"，
// 为了一行说明再抛第二个错误只会盖掉前一个
#![allow(clippy::let_underscore_must_use)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 被遵从的产物：一个带 `DISKEDIT_STATE_DIR`、stdin 关掉的 CLI 调用
pub fn disk_edit() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_DiskEdit"));
    command.stdin(Stdio::null());
    command
}

/// 跑完一整轮，收齐 stdout/stderr。`state` 是这台机器上的状态根
pub fn run(state: &Path, args: &[&str]) -> (i32, String, String) {
    let out = disk_edit()
        .args(args)
        .env("DISKEDIT_STATE_DIR", state)
        .output()
        .expect("spawn DiskEdit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 只有自己能进去的目录：状态根与镜像都不该被同机的其他用户碰到
pub fn private_dir(dir: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).expect("create the directory");
}

/// 缺失环境时的唯一出口：跳过必须可闻，并写出补验方式。
///
/// 直接写 `std::io::stderr`、不走 `eprintln!`：libtest 会把测试里的 `print` 宏收进捕获
/// 缓冲、成功时丢弃，于是"没有执行"在报告里与"通过"长得一模一样。这行 `note:` 是判读
/// 那两件事的唯一线索，必须穿透捕获
pub fn skip(target: &str, what: &str, why: &str) {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr(),
        "note: not executed {what}: {why}\n  verify with: DISKEDIT_VM_TESTS=1 \
         DISKEDIT_VM_BUNDLE=<bundle> cargo test --test {target} -- --test-threads=1"
    );
    assert!(
        std::env::var_os("DISKEDIT_REQUIRE_CAPABILITIES").is_none(),
        "{what} could not be exercised: {why} — this environment declared the capability, \
         so a silent skip is a bug in the test or the environment"
    );
}

/// 载体用例的统一前置：显式开启、有 bundle、有 qemu。三者缺一即跳过
pub fn gate(target: &str, what: &str) -> bool {
    if std::env::var_os("DISKEDIT_VM_TESTS").is_none() {
        skip(target, what, "the carrier tests are opt-in: they need a bundle and QEMU");
        return false;
    }
    if std::env::var_os("DISKEDIT_VM_BUNDLE").is_none() {
        skip(target, what, "DISKEDIT_VM_BUNDLE is not set, so placement cannot pick the VM carrier");
        return false;
    }
    if !Command::new("sh")
        .args(["-c", "command -v qemu-system-x86_64 || command -v qemu-system-aarch64"])
        .output()
        .is_ok_and(|o| o.status.success())
    {
        skip(target, what, "no qemu-system binary is on PATH");
        return false;
    }
    true
}

/// CLI 直接拉起 QEMU，所以载体就是 CLI 的子进程
pub fn qemu_child_of(cli: u32) -> Option<u32> {
    let out = Command::new("pgrep").args(["-P", &cli.to_string()]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()
}

/// 全机的 QEMU 进程：用来判"会话结束后有没有留下孤儿"
pub fn qemu_pids() -> Vec<u32> {
    let out = Command::new("pgrep").args(["-a", "qemu-system"]).output().expect("run pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

pub fn interrupt(pid: u32, signal: libc::c_int) {
    // SAFETY: 只向本用例自己拉起的进程发一个信号；失败（已退出）由调用方的等待覆盖
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

/// 等一个事件，超时即失败。等待窗口由调用方给出，不写死在 helpers 里
pub fn wait_until(limit: std::time::Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    ready()
}

/// root 身份跑一轮。`-u DISKEDIT_VM_BUNDLE` 让这一轮即使完全没有载体也成立——原生腿的
/// 理由是有能力，不是有载体
pub fn run_root(state: &Path, args: &[&str]) -> (i32, String, String) {
    let out = root_command(state, args).output().expect("run DiskEdit as root");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// 与 [`run_root`] 同一条命令，但留给孩子自己跑：调用方要一边等它一边观察进程
pub fn root_command(state: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("sudo");
    command
        .args(["-n", "--", "env"])
        .arg(format!("DISKEDIT_STATE_DIR={}", state.display()))
        .args(["-u", "DISKEDIT_VM_BUNDLE"])
        .arg(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .stdin(Stdio::null());
    command
}

// ---- 载体用例共用的画布 ------------------------------------------------------

/// 画布：64 MiB 镜像、32 MiB 的 ext4 分区 1、8 MiB 的纯数据分区 2。扩分区 1 四 MiB 就要
/// 求分区 2 右移同样距离——差分与中断点两条用例共用这段故事，所以几何只写这一处。
/// （`tests/vm_crash.rs` 的夹具另有 fd 交棒与假文件，故事不同，不共用这一份）
pub const IMAGE_BYTES: u64 = 64 * 1024 * 1024;
pub const SECTOR_BYTES: u64 = 512;
/// 每次 resize 请求的增长量：分区 1 之后的分区要右移同样距离
pub const GROW_MIB: u64 = 4;

const PART1_MIB: u64 = 32;
const PART2_MIB: u64 = 8;
/// ext4 超级块在分区内的偏移，以及 `s_log_block_size` / `s_blocks_count_{lo,hi}`
const EXT4_SB_OFFSET: u64 = 1024;
const EXT4_LOG_BLOCK_SIZE: usize = 0x18;
const EXT4_BLOCKS_LO: usize = 0x04;
const EXT4_BLOCKS_HI: usize = 0x150;

pub struct Canvas {
    pub dir: PathBuf,
    pub image: PathBuf,
    /// 铺画布用的状态根；用例自己的作业另开状态根
    pub state: PathBuf,
    pub first_lba: u64,
    pub last_lba: u64,
}

impl Canvas {
    /// 铺一块画布：建 GPT、两个分区、分区 1 上的 ext4。`tag` 只用于临时目录名
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("diskedit_canvas_{tag}_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        private_dir(&dir);
        let image = dir.join("work.img");
        std::fs::write(&image, vec![0u8; IMAGE_BYTES as usize]).expect("create the image");
        let state = dir.join("canvas-state");
        private_dir(&state);
        let p = image.to_str().expect("utf-8 path");
        for layout in [
            vec!["new".to_string(), p.to_string(), "--table".into(), "gpt".into(), "--yes".into()],
            vec!["add".to_string(), p.to_string(), format!("{PART1_MIB}M"), "--yes".into()],
            vec!["add".to_string(), p.to_string(), format!("{PART2_MIB}M"), "--yes".into()],
        ] {
            let args: Vec<&str> = layout.iter().map(String::as_str).collect();
            let (code, out, err) = run(&state, &args);
            assert_eq!(code, 0, "{args:?} must succeed: {out}\n{err}");
        }
        // 建 FS 由 root 做：这一步只是准备画布，不属于被判定的作业
        let (code, out, err) = run_root(&state, &["mkfs", &format!("{p}:1"), "ext4", "--yes"]);
        assert_eq!(code, 0, "the canvas needs an ext4 partition: {out}\n{err}");

        let (first_lba, last_lba) = Self::read_target_range(&state, &image);
        Self { dir, image, state, first_lba, last_lba }
    }

    /// 一份逐字节相同的拷贝：两条腿各有自己的镜像
    pub fn duplicate(&self, name: &str) -> PathBuf {
        let copy = self.dir.join(format!("{name}.img"));
        std::fs::copy(&self.image, &copy).expect("copy the canvas image");
        copy
    }

    /// 每个用例自己的状态根：记录落点不该被另一条腿看见
    pub fn private_state(&self, name: &str) -> PathBuf {
        let state = self.dir.join(format!("{name}-state"));
        private_dir(&state);
        state
    }

    /// 分区 1 起点与长度的字节口径：分界线之内只比 FS 几何，之外逐字节比
    pub fn head_end(&self) -> u64 {
        self.first_lba * SECTOR_BYTES
    }

    pub fn tail_start(&self) -> u64 {
        (self.last_lba + 1) * SECTOR_BYTES
    }

    pub fn part1_bytes(&self) -> u64 {
        self.tail_start() - self.head_end()
    }

    /// 扩 [`GROW_MIB`] 之后分区 1 的末扇区
    pub fn grown_last_lba(&self) -> u64 {
        self.last_lba + GROW_MIB * 1024 * 1024 / SECTOR_BYTES
    }

    /// 扩 [`GROW_MIB`] 之后分区 1 的字节数
    pub fn grown_part1_bytes(&self) -> u64 {
        (self.grown_last_lba() + 1) * SECTOR_BYTES - self.head_end()
    }

    pub fn head(&self, image: &Path) -> Vec<u8> {
        read_range(image, 0, self.head_end())
    }

    pub fn tail(&self, image: &Path, size: u64) -> Vec<u8> {
        read_range(image, self.tail_start(), size - self.tail_start())
    }

    /// 分区 1 里的 FS 几何：块大小 × 块数，即它自报用掉了多少字节
    pub fn geometry(&self, image: &Path) -> (u64, u64) {
        let sb = read_range(image, self.head_end() + EXT4_SB_OFFSET, 0x200);
        let log_block =
            u32::from_le_bytes(sb[EXT4_LOG_BLOCK_SIZE..EXT4_LOG_BLOCK_SIZE + 4].try_into().unwrap());
        let lo = u32::from_le_bytes(sb[EXT4_BLOCKS_LO..EXT4_BLOCKS_LO + 4].try_into().unwrap()) as u64;
        let hi =
            u32::from_le_bytes(sb[EXT4_BLOCKS_HI..EXT4_BLOCKS_HI + 4].try_into().unwrap()) as u64;
        let block_bytes = 1024u64 << log_block;
        (block_bytes, hi << 32 | lo)
    }

    /// 盘上分区 1 现在占的扇区区间。读的是盘，不是记录
    pub fn live_range(&self, state: &Path, image: &Path) -> (u64, u64) {
        Self::read_target_range(state, image)
    }

    fn read_target_range(state: &Path, image: &Path) -> (u64, u64) {
        let (code, out, err) = run(state, &["info", image.to_str().expect("utf-8 path")]);
        assert_eq!(code, 0, "info must succeed: {err}");
        let json: serde_json::Value =
            serde_json::from_str(&out).expect("info emits one JSON object");
        let part = &json["partitions"][0];
        (
            part["first_lba"].as_u64().expect("part 1 first_lba"),
            part["last_lba"].as_u64().expect("part 1 last_lba"),
        )
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// 两条用例共用的请求形状：扩分区 1 四 MiB，允许搬移，逐块推进
pub fn resize_args(image: &Path) -> Vec<String> {
    vec![
        "resize".into(),
        format!("{}:1", image.display()),
        format!("+{GROW_MIB}M"),
        "--allow-move".into(),
        "--chunk".into(),
        "1".into(),
        "--yes".into(),
    ]
}

pub fn read_range(path: &Path, offset: u64, len: u64) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).expect("open the image");
    file.seek(SeekFrom::Start(offset)).expect("seek");
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf).expect("read");
    buf
}

/// 第一处不同字节的绝对位置；逐字节相同则 `None`
pub fn first_difference(a: &[u8], b: &[u8], base: u64) -> Option<u64> {
    a.iter().zip(b).position(|(x, y)| x != y).map(|at| base + at as u64)
}
