#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败（临时目录/文件）
//! 端到端：gptman 造 GPT 镜像 → 真实二进制 `info` 读回 → JSON 含预期字段。

use std::io::Cursor;
use std::process::Command;

/// 夹具目录：复用 tempfile 后端，析构时重试删除。
struct FixtureDir(tempfile::TempDir);

impl FixtureDir {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("diskedit_fixture_")
            .tempdir()
            .expect("cannot create the fixture directory");
        Self(dir)
    }
}

impl std::ops::Deref for FixtureDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        self.0.path()
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        for _ in 0..3 {
            match std::fs::remove_dir_all(self.0.path()) {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => {}
            }
        }
    }
}

/// 落盘伴随文件名的词表与生成宏。跨 crate 无法共享，故在这里维护一份。
#[cfg(unix)]
macro_rules! sidecar_suffix {
    ($stem:literal) => {
        concat!(".diskedit.", $stem)
    };
}

/// 旧 undo journal 的落点名。
#[cfg(unix)]
const JOURNAL_SUFFIX: &str = sidecar_suffix!("journal");
/// 恢复记录目录（记录与目标锁都在里面）
#[cfg(unix)]
const RECORD_DIR_SUFFIX: &str = ".diskedit";
#[cfg(unix)]
const RECORD_LOCK_NAME: &str = "record.lock";

/// 目标旁边的伴随文件：`dir` 下的 `<name><suffix>`
#[cfg(unix)]
fn sidecar(dir: &std::path::Path, name: &str, suffix: &str) -> std::path::PathBuf {
    dir.join(format!("{name}{suffix}"))
}

/// 恢复记录目录的落点（`<image>.diskedit`）：记录与目标锁都在里面。
/// 只用于"拒绝后不得留下恢复状态"与"占住锁"这两类夹具动作，状态判据一律读 `inspect`
#[cfg(unix)]
fn record_dir(img: &std::path::Path) -> std::path::PathBuf {
    let mut os = img.as_os_str().to_owned();
    os.push(RECORD_DIR_SUFFIX);
    std::path::PathBuf::from(os)
}

/// 目录条目名（排序后），不含只增、不参与恢复的审计日志：
/// 拒绝路径不得新增 journal/ckpt 等恢复伴随物
#[cfg(unix)]
fn dir_entries(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        // 日志与记录目录是常驻现场（释放锁不删文件），不计入"拒绝后新增的伴随文件"
        .filter(|n| !n.ends_with(".log") && !n.ends_with(".diskedit"))
        .collect();
    names.sort();
    names
}

/// 写入型用例的目标平台是 Linux，`info` 的 JSON 用例同样只在 Linux 上可观察；
/// 非 unix 构建不引用这份镜像
#[cfg(unix)]
fn fixture_gpt_image(path: &std::path::Path) {
    let ss = 512u64;
    let data = vec![0u8; 100 * ss as usize];
    let mut cur = Cursor::new(data);
    let mut gpt = gptman::GPT::new_from(&mut cur, ss, [0xAB; 16]).unwrap();
    gpt[1] = gptman::GPTPartitionEntry {
        partition_type_guid: [
            0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
        ], // linux swap（混合端磁盘序）
        unique_partition_guid: [0x02; 16],
        starting_lba: 34,
        ending_lba: 60,
        attribute_bits: 0,
        partition_name: "smoke".into(),
    };
    gpt.write_into(&mut cur).unwrap();
    std::fs::write(path, with_protective_mbr(cur.into_inner())).unwrap();
}

/// 补保护 MBR（LBA0 槽位 1 = 0xEE、StartLBA = 1，签名 0x55AA）：
/// gptman 只写 GPT 结构，而 load_gpt 以保护 MBR 为判定前置（UEFI 2.10 §5.2.3）
fn with_protective_mbr(mut data: Vec<u8>) -> Vec<u8> {
    let total = data.len() as u64 / 512;
    let size: u32 = if total > u32::MAX as u64 { u32::MAX } else { (total - 1) as u32 };
    data[446 + 4] = 0xEE;
    data[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    data[446 + 12..446 + 16].copy_from_slice(&size.to_le_bytes());
    data[510] = 0x55;
    data[511] = 0xAA;
    data
}

/// 崩溃恢复。注入点只在 `test-faults` 构建下存在，故整块按 feature 隔离：
/// **默认 `cargo test` 不会跑这里**，要跑得显式 `cargo test --features test-faults`。
///
/// `resize-part` / `copy` 各有自己的收尾形状——一个留下可续跑的 ckpt，
/// 一个什么都不留、且已越过不可回滚点。断言必须落在它们**各自的**后效上
#[cfg(all(unix, feature = "test-faults"))]
mod crash_recovery;

/// 单文件恢复记录的三档崩溃注入：记录的落盘与目标写入、提交之间各断一次
#[cfg(all(unix, feature = "test-faults"))]
mod record;

#[cfg(not(target_os = "linux"))]
mod platform_gate;
// 写入型用例的目标平台是 Linux：非 Linux 由统一能力门拒绝全部命令，
// 这些用例在那里没有可观察的成功路径
#[cfg(target_os = "linux")]
mod commands;
#[cfg(target_os = "linux")]
mod confirm_gate;
mod guards;
// info 的可观察成功路径只在 Linux 上：非 Linux 由统一能力门拒绝
#[cfg(target_os = "linux")]
mod info;
#[cfg(target_os = "linux")]
mod journal;
#[cfg(unix)]
mod resize;
#[cfg(target_os = "linux")]
mod table;