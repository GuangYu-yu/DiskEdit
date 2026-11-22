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

/// 记录目录里的锁文件名。跨 crate 无法共享，故在这里维护一份。
#[cfg(unix)]
const RECORD_LOCK_NAME: &str = "record.lock";

/// 本次测试线程的镜像状态根：同一用例内的多次调用（含子进程）落点一致，用例之间互不相干。
///
/// `thread_local`：用例在派生线程里再调用时会各自建一个根；只解析参数的探针不落记录，
/// 分根对它们没有影响。
///
/// 进程内所有用例共用一个根时，临时镜像会复用内核回收的 inode，后来建出的镜像
/// 因此命中前一个用例留下的记录目录（记录、`.abandoned.N`、残留文件名）
fn state_root() -> std::path::PathBuf {
    thread_local! {
        static ROOT: tempfile::TempDir = {
            let dir = tempfile::tempdir().expect("a state root for this test");
            // 写命令复核状态根本身：记录里有写前分区表，别的本地账户不能读
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            dir
        };
    }
    ROOT.with(|d| d.path().to_path_buf())
}

/// 驱动真实二进制的命令：状态根固定到本用例的临时根，
/// 写命令的记录与锁落在那里，不碰运行测试的用户目录
fn diskedit() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_DiskEdit"));
    cmd.env("DISKEDIT_STATE_DIR", state_root());
    cmd
}

/// 本机能否**以 root 直接跑**需要内核权限的步骤（losetup/mkfs/resize 那一类）。
///
/// 只看真实 UID，不看载体：配了 `DISKEDIT_VM_BUNDLE` 的机器上生产会选 Vm，结局由载体
/// 决定，与这里判的"原生腿"不是同一件事。判据名要说明它量的是什么——用到它的用例必须
/// 自己排除"有载体"的现场（见 `resize` 里那两条的前置断言），否则就成了"名字说无载体、
/// 函数只管 UID"的判据名不副实
#[cfg(target_os = "linux")]
fn host_runs_fs_steps_natively() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).map(str::to_string))
        .and_then(|l| l.split_whitespace().nth(2).map(|f| f == "0"))
        .unwrap_or(false)
}

/// 镜像键目录的落点（`<image_state_root>/images/<键摘要>`）：记录与目标锁都在里面。
/// 只做路径计算，不建目录；夹具用它占住锁、读锁或断言目录为空，状态判据一律读 `inspect`
#[cfg(unix)]
fn record_dir(img: &std::path::Path) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(img).unwrap();
    let key = format!("inode:{}:{}", meta.dev(), meta.ino());
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    state_root().join("images").join(hex)
}

/// 目录条目名（排序后），不含只增、不参与恢复的审计日志：
/// 拒绝路径不得新增记录、锁等恢复伴随物
#[cfg(unix)]
fn dir_entries(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        // 记录落在每用户状态根，目标旁边只剩审计日志这类常驻现场
        .filter(|n| !n.ends_with(".log"))
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
/// `resize --end` / `copy` 各有自己的收尾形状——一个留下可续跑的 ckpt，
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
mod worker;
// info 的可观察成功路径只在 Linux 上：非 Linux 由统一能力门拒绝
#[cfg(target_os = "linux")]
mod info;
#[cfg(target_os = "linux")]
mod journal;
#[cfg(unix)]
mod resize;
#[cfg(target_os = "linux")]
mod table;