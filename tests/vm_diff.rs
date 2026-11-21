//! 差分验收：同一份作业，两条放置路径必须给出同一个结果。
//!
//! 判据：`resize img:1 +4M --allow-move` 在"非 root（无权限 ⇒ 交给载体）"与
//! "root（有权限有工具 ⇒ 原生执行）"两种身份上各跑一次，起点是同一份镜像的两份拷贝。
//! 三样必须一致——分区 1 分界线之外的字节（含主备 GPT 与分区 2 的数据）、分区 1 里的
//! FS 几何（块大小 × 块数，且等于分区长度）、退出码。
//!
//! 放置不是假设，是观察到的：载体那条腿必须出现 QEMU 子进程，原生那条腿在整轮里
//! 一次都不许出现。同一编译、同一参数、同一镜像，两次运行只差身份——这就是"差分"。
//!
//! **没有记录侧的逐字段比较**：成功收尾会删除记录（`recovery::cleanup`），留下的可比对
//! 物只有磁盘字节；记录里唯一与放置有关的是审计尾段，而它正是被排除的那一段。
//! 因此记录侧的事实由"载体腿出现 QEMU、原生腿没有"这一对观察承担。
//!
//! 关于"VM 记录由原生续跑"：记录里没有放置字段，审计字段不参与续跑判定，
//! 所以那不是另一种情形，它就是"任意记录被原生续跑"，由
//! `tests/worker_protocol.rs::resume_after_kill_before_ack`（协议层）与
//! `src/recovery/record.rs::tests::old_magic_is_not_ours`（记录层）钉住，本文件不重复。
//!
//! 前置：QEMU、真实 bundle、非 root 身份、免密 `sudo`。缺一即打印补验方式后跳过；
//! `DISKEDIT_REQUIRE_CAPABILITIES` 置位时跳过即失败。

#![cfg(target_os = "linux")]

mod common;

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// 画布：64 MiB 镜像；分区 1 是 32 MiB 的 ext4，分区 2 是 8 MiB 的纯数据。
/// 扩分区 1 四 MiB 就要求分区 2 右移同样距离——与 `tests/vm_crash.rs` 的夹具同一几何
const IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const PART1_MIB: u64 = 32;
const PART2_MIB: u64 = 8;
const SECTOR_BYTES: u64 = 512;

/// ext4 超级块在分区内的偏移；`s_log_block_size` 与 `s_blocks_count_{lo,hi}`
const EXT4_SB_OFFSET: usize = 1024;
const EXT4_LOG_BLOCK_SIZE: usize = 0x18;
const EXT4_BLOCKS_LO: usize = 0x04;
const EXT4_BLOCKS_HI: usize = 0x150;

struct Fixture {
    dir: PathBuf,
    /// 载体腿的镜像与状态根
    carrier: PathBuf,
    carrier_state: PathBuf,
    /// 原生腿的镜像与状态根
    native: PathBuf,
    native_state: PathBuf,
    /// 分区 1 的字节区间：分界线之内只比 FS 几何，之外逐字节比
    head_end: u64,
    tail_start: u64,
    part1_bytes: u64,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("diskedit_vm_diff_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        common::private_dir(&dir);
        let image = dir.join("work.img");
        std::fs::write(&image, vec![0u8; IMAGE_BYTES as usize]).expect("create the image");

        let p = image.to_str().expect("utf-8 path");
        let state = dir.join("work-state");
        common::private_dir(&state);
        for layout in [
            vec!["new".to_string(), p.to_string(), "--table".into(), "gpt".into(), "--yes".into()],
            vec!["add".to_string(), p.to_string(), format!("{PART1_MIB}M"), "--yes".into()],
            vec!["add".to_string(), p.to_string(), format!("{PART2_MIB}M"), "--yes".into()],
        ] {
            let args: Vec<&str> = layout.iter().map(String::as_str).collect();
            let (code, out, err) = common::run(&state, &args);
            assert_eq!(code, 0, "{args:?} must succeed: {out}\n{err}");
        }

        // 建 FS 由 root 做：这一步只是准备画布，不属于被判定的作业
        let (code, out, err) = run_root(&state, &["mkfs", &format!("{p}:1"), "ext4", "--yes"]);
        assert_eq!(code, 0, "the fixture needs an ext4 partition: {out}\n{err}");

        let (code, out, err) = common::run(&state, &["info", p]);
        assert_eq!(code, 0, "info must succeed: {err}");
        let json: serde_json::Value = serde_json::from_str(&out).expect("info emits one JSON object");
        let part1 = &json["partitions"][0];
        let first_lba = part1["first_lba"].as_u64().expect("part 1 first_lba");
        let last_lba = part1["last_lba"].as_u64().expect("part 1 last_lba");
        let head_end = first_lba * SECTOR_BYTES;
        let tail_start = (last_lba + 1) * SECTOR_BYTES;
        let part1_bytes = tail_start - head_end;

        // 两份逐字节相同的起点：一份交给载体，一份交给原生
        let carrier = dir.join("carrier.img");
        let native = dir.join("native.img");
        std::fs::copy(&image, &carrier).expect("copy the carrier image");
        std::fs::copy(&image, &native).expect("copy the native image");
        std::fs::remove_file(&image).ok();

        let carrier_state = dir.join("carrier-state");
        let native_state = dir.join("native-state");
        common::private_dir(&carrier_state);
        common::private_dir(&native_state);

        Self { dir, carrier, carrier_state, native, native_state, head_end, tail_start, part1_bytes }
    }

    fn head(&self, image: &Path) -> Vec<u8> {
        read_range(image, 0, self.head_end)
    }

    fn tail(&self, image: &Path, size: u64) -> Vec<u8> {
        read_range(image, self.tail_start, size - self.tail_start)
    }

    /// 分区 1 里的 FS 几何：块大小 × 块数，即它自报用掉了多少字节
    fn geometry(&self, image: &Path) -> (u64, u64) {
        let sb = read_range(image, self.head_end + EXT4_SB_OFFSET as u64, 0x200);
        let log_block = u32::from_le_bytes(sb[EXT4_LOG_BLOCK_SIZE..EXT4_LOG_BLOCK_SIZE + 4].try_into().unwrap());
        let lo = u32::from_le_bytes(sb[EXT4_BLOCKS_LO..EXT4_BLOCKS_LO + 4].try_into().unwrap()) as u64;
        let hi = u32::from_le_bytes(sb[EXT4_BLOCKS_HI..EXT4_BLOCKS_HI + 4].try_into().unwrap()) as u64;
        let block_bytes = 1024u64 << log_block;
        (block_bytes, hi << 32 | lo)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn resize_args(image: &Path) -> Vec<String> {
    vec![
        "resize".into(),
        format!("{}:1", image.display()),
        "+4M".into(),
        "--allow-move".into(),
        "--chunk".into(),
        "1".into(),
        "--yes".into(),
    ]
}

fn read_range(path: &Path, offset: u64, len: u64) -> Vec<u8> {
    let mut file = std::fs::File::open(path).expect("open the image");
    file.seek(SeekFrom::Start(offset)).expect("seek");
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf).expect("read");
    buf
}

fn first_difference(a: &[u8], b: &[u8], base: u64) -> Option<u64> {
    a.iter().zip(b).position(|(x, y)| x != y).map(|at| base + at as u64)
}

/// root 身份跑一轮：状态根与镜像都按 root 的规矩来。`-u DISKEDIT_VM_BUNDLE` 让这一轮
/// 在完全没有载体的情况下也成立——原生腿的理由是有能力，不是有载体
fn run_root(state: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new("sudo")
        .args(["-n", "--", "env"])
        .arg(format!("DISKEDIT_STATE_DIR={}", state.display()))
        .args(["-u", "DISKEDIT_VM_BUNDLE"])
        .arg(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run DiskEdit as root");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn spawn_native(f: &Fixture) -> Child {
    let out = std::fs::File::create(f.dir.join("native.stdout")).expect("create the log");
    let err = std::fs::File::create(f.dir.join("native.stderr")).expect("create the log");
    Command::new("sudo")
        .args(["-n", "--", "env"])
        .arg(format!("DISKEDIT_STATE_DIR={}", f.native_state.display()))
        .args(["-u", "DISKEDIT_VM_BUNDLE"])
        .arg(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(resize_args(&f.native))
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("run DiskEdit as root")
}

/// 三样前置之外，还要"身份"这一样：root 会让两条腿都变成原生，差分就没了
fn gate(what: &str) -> bool {
    if !common::gate("vm_diff", what) {
        return false;
    }
    // SAFETY: geteuid 无参数、只读
    if unsafe { libc::geteuid() } == 0 {
        common::skip("vm_diff", what, "run as a non-root user: as root both legs would be native");
        return false;
    }
    if !Command::new("sudo")
        .args(["-n", "--", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        common::skip("vm_diff", what, "the native leg needs password-less sudo");
        return false;
    }
    true
}

#[test]
fn the_two_placements_agree_on_the_result() {
    if !gate("the_two_placements_agree_on_the_result") {
        return;
    }
    let f = Fixture::new();

    // 载体腿：非 root、有 bundle ⇒ 宿主能力不足，作业交给载体。QEMU 子进程是放置的正面证据
    let out_log = std::fs::File::create(f.dir.join("carrier.stdout")).expect("create the log");
    let err_log = std::fs::File::create(f.dir.join("carrier.stderr")).expect("create the log");
    let mut carrier = common::disk_edit()
        .args(resize_args(&f.carrier))
        .env("DISKEDIT_STATE_DIR", &f.carrier_state)
        .stdout(Stdio::from(out_log))
        .stderr(Stdio::from(err_log))
        .spawn()
        .expect("spawn DiskEdit");
    let pid = carrier.id();
    assert!(
        common::wait_until(Duration::from_secs(120), || common::qemu_child_of(pid).is_some()),
        "the non-root leg never started a carrier"
    );
    let status = carrier.wait().expect("the carrier leg ends");
    let carrier_err = std::fs::read_to_string(f.dir.join("carrier.stderr")).unwrap_or_default();
    assert_eq!(
        status.code().unwrap_or(-1),
        0,
        "the carrier leg must finish: {carrier_err}"
    );

    // 原生腿：root、有工具 ⇒ 宿主能力齐备，作业在宿主执行。整轮不得出现载体
    let mut native = spawn_native(&f);
    let mut saw_carrier = false;
    let native_status = loop {
        saw_carrier |= common::qemu_child_of(native.id()).is_some();
        if let Some(status) = native.try_wait().expect("query the native leg") {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let native_err = std::fs::read_to_string(f.dir.join("native.stderr")).unwrap_or_default();
    assert!(!saw_carrier, "the root leg started a carrier: the host could do the job itself");
    assert_eq!(native_status.code().unwrap_or(-1), 0, "the native leg must finish: {native_err}");

    let size = std::fs::metadata(&f.carrier).expect("carrier image").len();
    assert_eq!(
        std::fs::metadata(&f.native).expect("native image").len(),
        size,
        "the two runs must start and end on the same canvas"
    );

    // 分界线之外：主备 GPT、分区 2 的字节必须逐字节相同
    let head_a = f.head(&f.carrier);
    let head_b = f.head(&f.native);
    assert_eq!(
        first_difference(&head_a, &head_b, 0),
        None,
        "the two placements disagree on the bytes before the partition"
    );
    let tail_a = f.tail(&f.carrier, size);
    let tail_b = f.tail(&f.native, size);
    assert_eq!(
        first_difference(&tail_a, &tail_b, f.tail_start),
        None,
        "the two placements disagree on the bytes after the partition"
    );

    // 分界线之内：FS 几何相同，且两条腿都把 FS 扩到了分区末尾
    let (block_a, blocks_a) = f.geometry(&f.carrier);
    let (block_b, blocks_b) = f.geometry(&f.native);
    assert_eq!(
        (block_a, blocks_a),
        (block_b, blocks_b),
        "the two placements disagree on the filesystem geometry"
    );
    assert_eq!(
        block_a * blocks_a,
        f.part1_bytes,
        "the filesystem did not grow to fill the partition: {blocks_a} blocks of {block_a} B \
         against a partition of {} B",
        f.part1_bytes
    );
}
