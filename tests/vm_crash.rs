//! P3 崩溃矩阵：只有真实载体才能回答的判据——宿主与 guest 交棒、ack 之后的持久性、
//! carrier 被杀留下的不确定结论。
//!
//! 默认不跑：要 QEMU，也要一个能引导的 bundle。在 Linux 主机上手动执行，
//! 命令与环境前提见 `tools/vm/README.md` 的 "Running the carrier tests"：
//!
//! ```text
//! DISKEDIT_VM_TESTS=1 DISKEDIT_VM_BUNDLE=/path/to/bundle \
//! cargo test --test vm_crash -- --test-threads=1
//! ```
//!
//! **放置必须真的落到载体**，否则这些用例只是在原生路径上自说自话。外部工具搜索路径
//! 是编译期常量（`fsops::env::TOOL_PATH`，不读 `PATH`），所以跑这套用例的二进制要用
//! `DISKEDIT_TOOL_PATH=/nonexistent` 编译：宿主报"缺少 FS 工具"，含 FS 步的作业只能
//! 交给载体。这也是这些用例必须**带 FS 步**（`mkfs` / `resize`）的原因——纯 `move` 在
//! 宿主永远做得成，放置恒为 Native，测不到载体。
//!
//! 不需要载体的那一半事实在更靠近产生点的位置另有判据，本文件不重复：检查点与 ack 的
//! 先后、被杀后的断点续跑见 `tests/worker_protocol.rs`；"路径被替换后写入仍落在已打开的
//! inode" 见 `src/dev/tests.rs::a_swapped_path_does_not_redirect_writes`；"交给 QEMU 的是
//! 宿主核对过的那个描述符" 见 `src/exec/qemu.rs::tests::no_unsafe_cache_and_exit_device`。
//!
//! 两条放置路径的一致性由 `tests/vm_diff.rs` 负责；"载体停在半途、原生接手"由
//! `tests/vm_interrupt.rs` 负责。两者的分工写在各自文件头：本文件只做崩溃现场。

#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant};

/// 现场：64 MiB 镜像；分区 1 是 32 MiB 的 ext4，分区 2 是 8 MiB 的纯数据。
/// 扩分区 1 四 MiB 就必须把分区 2 右移同样距离——位移小于分区，源与目的相交，
/// 搬移按块从尾到头
const IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const PART1_MIB: u64 = 32;
const PART2_MIB: u64 = 8;
const GROW_SECTORS: u64 = 4 * 1024 * 1024 / 512;
const CHUNK_BYTES: u64 = 1024 * 1024;
const SECTOR_BYTES: u64 = 512;

struct Fixture {
    dir: PathBuf,
    image: PathBuf,
    state: PathBuf,
    part1_lba: u64,
    part2_lba: u64,
    part2_sectors: u64,
}

impl Fixture {
    /// 完整现场：分区 1 上的 ext4 由载体建。宿主没有 mkfs，这一步只能交给载体；
    /// 它同时是"载体确实跑起来了"的第一道事实
    fn new(tag: &str) -> Self {
        let f = Self::build(tag);
        let p = f.image.to_str().expect("utf-8 path");
        let (code, out, err) = common::run(&f.state, &["mkfs", &format!("{p}:1"), "ext4", "--yes"]);
        assert_eq!(code, 0, "the carrier must create the ext4 partition: {out}\n{err}");
        f
    }

    /// 只有表与数据的现场：夹具自检用它，不需要载体
    fn build(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("diskedit_vm_{tag}_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        common::private_dir(&dir);
        let state = dir.join("state");
        common::private_dir(&state);
        let image = dir.join("disk.img");
        std::fs::write(&image, vec![0u8; IMAGE_BYTES as usize]).expect("create the image");

        let p = image.to_str().expect("utf-8 path");
        for layout in [
            vec!["new".to_string(), p.to_string(), "--table".into(), "gpt".into(), "--yes".into()],
            vec!["add".to_string(), p.to_string(), format!("{PART1_MIB}M"), "--yes".into()],
            vec!["add".to_string(), p.to_string(), format!("{PART2_MIB}M"), "--yes".into()],
        ] {
            let args: Vec<&str> = layout.iter().map(String::as_str).collect();
            let (code, out, err) = common::run(&state, &args);
            assert_eq!(code, 0, "{args:?} must succeed: {out}\n{err}");
        }

        let (code, out, err) = common::run(&state, &["info", p]);
        assert_eq!(code, 0, "info must succeed: {err}");
        let json: serde_json::Value = serde_json::from_str(&out).expect("info emits one JSON object");
        let parts = json["partitions"].as_array().expect("partitions");
        assert_eq!(parts.len(), 2, "{out}");
        let part1_lba = parts[0]["first_lba"].as_u64().expect("part 1 first_lba");
        let part2_lba = parts[1]["first_lba"].as_u64().expect("part 2 first_lba");
        let part2_last = parts[1]["last_lba"].as_u64().expect("part 2 last_lba");
        let part2_sectors = part2_last - part2_lba + 1;

        // 分区 2 铺可识别字节：搬移的判据是"这些字节出现在目的区间"
        let mut bytes = std::fs::read(&image).unwrap();
        let start = part2_lba as usize * SECTOR_BYTES as usize;
        let len = part2_sectors as usize * SECTOR_BYTES as usize;
        bytes[start..start + len].copy_from_slice(&pattern(len));
        std::fs::write(&image, &bytes).unwrap();

        Self { dir, image, state, part1_lba, part2_lba, part2_sectors }
    }

    /// 扩分区 1 四 MiB：分区 2 必须右移同样距离，搬移在载体里按块进行
    fn resize_args(&self) -> Vec<String> {
        vec![
            "resize".into(),
            format!("{}:1", self.image.display()),
            "+4M".into(),
            "--allow-move".into(),
            "--chunk".into(),
            "1".into(),
            "--yes".into(),
        ]
    }

    fn resize(&self) -> (i32, String, String) {
        let args = self.resize_args();
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        common::run(&self.state, &borrowed)
    }

    /// 拉起 CLI 但不收输出：`-serial stdio` 会把 guest 的串口输出汇进 CLI 的 stdout，
    /// 管道不读会填满并把 CLI 卡住——等待游标时正是不读的时候，所以落文件
    fn spawn(&self) -> Child {
        let out = std::fs::File::create(self.dir.join("run.stdout")).expect("create the log");
        let err = std::fs::File::create(self.dir.join("run.stderr")).expect("create the log");
        let mut command = common::disk_edit();
        command
            .args(self.resize_args())
            .env("DISKEDIT_STATE_DIR", &self.state)
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err));
        command.spawn().expect("spawn DiskEdit")
    }

    fn logs(&self) -> String {
        let out = std::fs::read_to_string(self.dir.join("run.stdout")).unwrap_or_default();
        let err = std::fs::read_to_string(self.dir.join("run.stderr")).unwrap_or_default();
        format!("stdout:\n{out}\nstderr:\n{err}")
    }

    /// 记录里已 acked 的搬移游标；没有记录时 `None`
    fn cursor(&self) -> Option<u64> {
        let path = self.image.to_str()?;
        let (code, out, _) = common::run(&self.state, &["inspect", path]);
        (code == 0).then_some(())?;
        let line = out.lines().find(|line| line.trim_start().starts_with("progress:"))?;
        let cursor = line.split_whitespace().find_map(|part| part.strip_prefix("cursor="))?;
        cursor.parse().ok()
    }

    fn wait_cursor(&self, n: u64) {
        if !common::wait_until(Duration::from_secs(180), || {
            self.cursor().is_some_and(|cursor| cursor >= n)
        }) {
            panic!("the record never reported {n} acknowledged chunk(s)\n{}", self.logs());
        }
    }

    /// 搬完之后分区 2 的落点：扩容四 MiB 就右移四 MiB
    fn part2_destination_lba(&self) -> u64 {
        self.part2_lba + GROW_SECTORS
    }

    /// 完成后分区 2 实际落点：用来确认上面那条夹具假设，而不是假设它
    fn part2_first_lba_now(&self) -> u64 {
        let (code, out, err) = common::run(&self.state, &["info", self.image.to_str().unwrap()]);
        assert_eq!(code, 0, "info must succeed: {err}");
        let json: serde_json::Value = serde_json::from_str(&out).expect("info emits one JSON object");
        json["partitions"][1]["first_lba"].as_u64().expect("part 2 first_lba")
    }

    fn destination_bytes(&self, from_sector: u64, sectors: u64) -> Vec<u8> {
        let start = (self.part2_destination_lba() + from_sector) as usize * SECTOR_BYTES as usize;
        let len = sectors as usize * SECTOR_BYTES as usize;
        std::fs::read(&self.image).unwrap()[start..start + len].to_vec()
    }

    fn assert_destination_matches(&self) {
        let expected = pattern(self.part2_sectors as usize * SECTOR_BYTES as usize);
        assert_eq!(
            self.destination_bytes(0, self.part2_sectors),
            expected,
            "the moved partition does not match the source bytes"
        );
    }

    /// 尾块的期望字节：右移时搬移从尾到头，"已 ack"的证据在目的区间的尾部
    fn expected_tail(&self) -> (u64, Vec<u8>) {
        let chunk_sectors = CHUNK_BYTES / SECTOR_BYTES;
        let tail_from = self.part2_sectors - chunk_sectors;
        let mut expected = pattern(self.part2_sectors as usize * SECTOR_BYTES as usize);
        let tail = expected.split_off(tail_from as usize * SECTOR_BYTES as usize);
        (tail_from, tail)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// 夹具自检：不需要载体，把几何读取、分区 2 的字节模式与目的区间的算式走一遍。
/// 六条载体用例共用这些帮助函数，它们自己必须先被跑过
#[test]
fn the_fixture_geometry_is_movable() {
    let f = Fixture::build("fixture_selfcheck");
    let p = f.image.to_str().expect("utf-8 path");
    let destination = f.part2_destination_lba().to_string();
    let (code, out, err) = common::run(
        &f.state,
        &["move", &format!("{p}:2"), "--start", &destination, "--chunk", "1", "--yes"],
    );
    assert_eq!(code, 0, "a native move of partition 2 must succeed: {out}\n{err}");
    f.assert_destination_matches();
    assert!(f.part2_sectors > 0 && f.part1_lba < f.part2_lba, "the fixture geometry is unusable");
}

/// ack 之后被杀：已 ack 的块必须已经在盘上，记录停在该游标，重跑不跳过这些块
#[test]
fn acked_data_survives_qemu_kill() {
    if !common::gate("vm_crash", "acked_data_survives_qemu_kill") {
        return;
    }
    let f = Fixture::new("acked");
    let mut child = f.spawn();
    f.wait_cursor(1);
    let pid = child.id();
    child.kill().ok();
    child.wait().ok();

    let (tail_from, expected) = f.expected_tail();
    assert_eq!(
        f.destination_bytes(tail_from, CHUNK_BYTES / SECTOR_BYTES),
        expected,
        "the acknowledged tail chunk was not on disk after the carrier was killed"
    );
    assert!(
        f.cursor().is_some_and(|cursor| cursor >= 1),
        "the record must keep the acknowledged cursor\n{}",
        f.logs()
    );

    let (code, _, err) = f.resize();
    assert_eq!(code, 0, "the resumed run must finish: {err}\n{}", f.logs());
    f.assert_destination_matches();
    assert_eq!(common::qemu_child_of(pid), None, "the killed session left a QEMU child behind");
}

/// guest 自己退出即结束会话：协议结果与提交都到齐，宿主不必靠超时收场
#[test]
fn guest_exit_terminates_qemu() {
    if !common::gate("vm_crash", "guest_exit_terminates_qemu") {
        return;
    }
    let f = Fixture::new("exit");
    let baseline = common::qemu_pids().len();
    let started = Instant::now();
    let (code, out, err) = f.resize();
    let elapsed = started.elapsed();
    assert_eq!(code, 0, "the VM run must complete: {out}\n{err}");
    assert!(
        elapsed < Duration::from_secs(60),
        "the run took {elapsed:?} — a hung guest cannot produce a result frame, so this ended \
         on a timeout rather than on the guest's own exit"
    );
    assert_eq!(common::qemu_pids().len(), baseline, "a QEMU process outlived the session");
    assert_eq!(
        f.part2_first_lba_now(),
        f.part2_destination_lba(),
        "扩了 4 MiB 却没有把分区 2 右移 4 MiB：夹具的几何假设不成立，其余用例的目的区间算式一并失效"
    );
    f.assert_destination_matches();
}

/// 文件系统工具在 guest 里半途被杀：宿主拿不到确定结论，只能按 `Unknown` 收尾——
/// 记录保留、退出 5
#[test]
fn fs_tool_killed_is_unknown() {
    if !common::gate("vm_crash", "fs_tool_killed_is_unknown") {
        return;
    }
    let f = Fixture::build("fstool");
    let p = f.image.to_str().expect("utf-8 path");

    // 建 FS 是纯 FS 步：宿主没有 mke2fs，只能由载体执行
    let mut child = common::disk_edit()
        .args(["mkfs", &format!("{p}:1"), "ext4", "--yes"])
        .env("DISKEDIT_STATE_DIR", &f.state)
        .spawn()
        .expect("spawn DiskEdit");
    let pid = child.id();
    assert!(
        common::wait_until(Duration::from_secs(60), || common::qemu_child_of(pid).is_some()),
        "the VM session never started"
    );
    let qemu = common::qemu_child_of(pid).expect("the VM session must have a QEMU child");
    common::interrupt(qemu, libc::SIGKILL);

    let status = child.wait().expect("the host exits on its own");
    assert_eq!(
        status.code().unwrap_or(-1),
        5,
        "a carrier killed mid-step must end as unknown, not as success"
    );
    let (code, out, err) = common::run(&f.state, &["inspect", p]);
    assert_eq!(code, 0, "inspect must still read the record: {err}");
    assert!(out.contains("progress:"), "the record must survive an unknown outcome: {out}");
}

/// SIGINT：QEMU 一并终止，不留孤儿，记录停在最后已 ack 的检查点
#[test]
fn sigint_leaves_no_orphan() {
    if !common::gate("vm_crash", "sigint_leaves_no_orphan") {
        return;
    }
    let f = Fixture::new("sigint");
    let mut child = f.spawn();
    f.wait_cursor(1);
    let pid = child.id();
    common::interrupt(pid, libc::SIGINT);
    let status = child.wait().expect("the host exits after SIGINT");
    assert!(status.code().is_some(), "the host must exit rather than be killed by the signal");

    // PDEATHSIG 是 SIGKILL：孤儿最多存在一个调度窗口
    assert!(
        common::wait_until(Duration::from_secs(5), || common::qemu_child_of(pid).is_none()),
        "SIGINT left a QEMU orphan"
    );
    assert!(f.cursor().is_some(), "the record must survive a SIGINT");
}

/// fdset × 缓存：`-add-fd` 若把缓存退回 writeback，ack 过的字节在 QEMU 被 SIGKILL 后
/// 会丢。判据是"ack 之后立刻 SIGKILL，已 ack 的字节仍在盘上"——持久性优先，
/// 不成立就必须放弃 fdset 走退回路径（结论写进交付）
#[test]
fn fdset_cache_combination() {
    if !common::gate("vm_crash", "fdset_cache_combination") {
        return;
    }
    let f = Fixture::new("fdset");
    let mut child = f.spawn();
    f.wait_cursor(1);
    let pid = child.id();
    let qemu = common::qemu_child_of(pid).expect("the VM session must have a QEMU child");
    // 只杀 QEMU，不杀宿主：判定的是"字节到底有没有落盘"
    common::interrupt(qemu, libc::SIGKILL);
    child.wait().ok();

    let (tail_from, expected) = f.expected_tail();
    assert_eq!(
        f.destination_bytes(tail_from, CHUNK_BYTES / SECTOR_BYTES),
        expected,
        "an acknowledged chunk vanished with the carrier: fdset forced the cache back to \
         writeback, so `-add-fd` cannot be used as configured"
    );
}
