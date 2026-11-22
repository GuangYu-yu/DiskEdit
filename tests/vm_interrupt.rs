//! 跨执行器接手：载体把记录停在"表已写、FS 未扩"，原生把它收完。
//!
//! 上面那几层判据都覆盖不到这个中间态——那个盘面只有真实执行才产生。这里只断言一件事：
//! 原生重跑同一条命令，退出 0，并且只把剩下的 FS 收尾做掉。
//!
//! 停点用既有注入点，不靠信号抢时间窗：`test-faults` 构建下
//! `DISKEDIT_FAULT=after-table-write` 让宿主在表步写完、记录尚未推进处 abort
//! （`src/movepart/faults.rs`）。于是现场必然是表已落、记录未推进、FS 未扩。
//!
//! 腿 1 落在载体上不是假设：整份作业的能力按并集取，FS 步要求 root，非 root 身份下连表步
//! 也交给载体；腿 1 结束后记录里 `audit: placement=Vm` 自证这一点。
//!
//! 目标是镜像文件：记录只由宿主写，两条腿要认出同一份记录，就得落在同一个记录根。镜像
//! 记录的根认 `DISKEDIT_STATE_DIR`（`dev::identity::image_state_root`），非 root 与 root
//! 都指向同一处；块设备记录的根（`dev::host_repo::host_state_root`）在非 root 时忽略
//! 这个变量，两条腿会分家。块设备共用同一份记录与判定代码，但造不出这个现场：非 root
//! 写不了块设备，root 又不会进载体。
//!
//! 前置：QEMU、真实 bundle、非 root 身份、免密 `sudo`，以及 `--features test-faults` 构建
//! （否则 `DISKEDIT_FAULT` 是空存根，腿 1 会跑完而不是停住）。缺一即跳过；
//! `DISKEDIT_REQUIRE_CAPABILITIES` 置位时跳过即失败。

#![cfg(target_os = "linux")]

mod common;

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const WHAT: &str = "a_carrier_interruption_is_finished_by_the_native_executor";

fn gate() -> bool {
    if !cfg!(feature = "test-faults") {
        common::skip(
            "vm_interrupt",
            WHAT,
            "this case stops the process at a declared fault point: build it with \
             `--features test-faults`, otherwise DISKEDIT_FAULT is an empty stub",
        );
        return false;
    }
    if !common::gate("vm_interrupt", WHAT) {
        return false;
    }
    // SAFETY: geteuid 无参数、只读
    if unsafe { libc::geteuid() } == 0 {
        common::skip(
            "vm_interrupt",
            WHAT,
            "run as a non-root user: as root leg 1 would be native and there would be no hand-off",
        );
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
        common::skip("vm_interrupt", WHAT, "the native leg needs password-less sudo");
        return false;
    }
    true
}

/// `abort()` 不走清理路径：把腿 1 留下的载体进程收掉，否则腿 2 的"整轮无载体"会被它污染
fn settle_carriers(before: &[u32]) {
    let leftovers = || -> Vec<u32> {
        common::qemu_pids().into_iter().filter(|pid| !before.contains(pid)).collect()
    };
    for pid in leftovers() {
        common::interrupt(pid, libc::SIGTERM);
    }
    if common::wait_until(Duration::from_secs(10), || leftovers().is_empty()) {
        return;
    }
    for pid in leftovers() {
        common::interrupt(pid, libc::SIGKILL);
    }
    assert!(
        common::wait_until(Duration::from_secs(10), || leftovers().is_empty()),
        "a carrier process outlived the interruption and could not be killed"
    );
}

fn logs(dir: &Path, name: &str) -> (std::fs::File, std::fs::File) {
    (
        std::fs::File::create(dir.join(format!("{name}.stdout"))).expect("create the log"),
        std::fs::File::create(dir.join(format!("{name}.stderr"))).expect("create the log"),
    )
}

/// 跑完一轮，把"这一轮有没有出现载体"一起观察回来
fn run_watched(mut child: Child) -> (std::process::ExitStatus, bool) {
    let mut saw_carrier = false;
    let status = loop {
        saw_carrier |= common::qemu_child_of(child.id()).is_some();
        if let Some(status) = child.try_wait().expect("query the leg") {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (status, saw_carrier)
}

#[test]
fn a_carrier_interruption_is_finished_by_the_native_executor() {
    if !gate() {
        return;
    }
    let canvas = common::Canvas::new("vm_interrupt");
    let image = canvas.image.clone();
    let state = canvas.private_state("job");
    let path = image.to_str().expect("utf-8 path");
    let arguments = common::resize_args(&image);
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();

    // 腿 1：非 root ⇒ 整份作业交给载体；闸在"表已写、记录未推进"
    let before = common::qemu_pids();
    let (out, err) = logs(&canvas.dir, "leg1");
    let leg1 = common::disk_edit()
        .args(&arguments)
        .env("DISKEDIT_STATE_DIR", &state)
        .env("DISKEDIT_FAULT", "after-table-write")
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn DiskEdit");
    let (leg1_status, saw_carrier) = run_watched(leg1);
    let leg1_err = std::fs::read_to_string(canvas.dir.join("leg1.stderr")).unwrap_or_default();
    assert!(saw_carrier, "leg 1 never started a carrier, so there is nothing to hand off: {leg1_err}");
    assert_eq!(
        leg1_status.signal(),
        Some(libc::SIGABRT),
        "the fault point must abort the host instead of finishing the job: {leg1_err}"
    );
    settle_carriers(&before);

    // 停点自证之一：这份记录由载体写下——否则本用例测的是原生对原生
    let (code, inspected, inspect_err) = common::run(&state, &["inspect", path]);
    assert_eq!(code, 0, "inspect must succeed: {inspect_err}");
    assert!(
        inspected.contains("audit: placement=Vm"),
        "leg 1 did not run on the carrier, so there is no hand-off to test: {inspected}"
    );

    // 停点自证之二：表已落、FS 未扩
    let (_, last_lba) = canvas.live_range(&state, &image);
    assert_eq!(
        last_lba,
        canvas.grown_last_lba(),
        "the table step did not land: partition 1 still ends at {last_lba}"
    );
    let (block, blocks) = canvas.geometry(&image);
    assert_eq!(
        block * blocks,
        canvas.part1_bytes(),
        "the filesystem already grew, so the fault point landed past the FS step"
    );

    // 腿 2：同一份镜像、同一条命令，root 身份 ⇒ 原生。整轮不得出现载体。
    // 记录目录归写下它的 euid，root 要接着写就得先接手；记录文件仍是目标属主（镜像属主
    // 是 tester），所以只换目录，别用 `chown -R`——文件易主会被归属检查拒掉
    let state_path = state.to_str().expect("utf-8 state path");
    let (chown_code, _, chown_err) =
        common::sudo(&["find", state_path, "-type", "d", "-exec", "chown", "0:0", "{}", "+"]);
    assert_eq!(chown_code, 0, "cannot hand the record directories over to root: {chown_err}");
    let (out, err) = logs(&canvas.dir, "leg2");
    let leg2 = common::root_command(&state, &arguments)
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .expect("run DiskEdit as root");
    let (leg2_status, saw_carrier) = run_watched(leg2);
    let leg2_err = std::fs::read_to_string(canvas.dir.join("leg2.stderr")).unwrap_or_default();
    assert!(!saw_carrier, "the native leg started a carrier: the record did not hand off");
    assert_eq!(leg2_status.code(), Some(0), "the native resume must finish the job: {leg2_err}");

    // 收尾没有丢东西：目标区间仍是记录里的那一个，FS 扩到了分区末尾
    let (_, last_lba) = canvas.live_range(&state, &image);
    assert_eq!(
        last_lba,
        canvas.grown_last_lba(),
        "the resumed job lost the recorded target: partition 1 ends at {last_lba}"
    );
    let (block, blocks) = canvas.geometry(&image);
    assert_eq!(
        block * blocks,
        canvas.grown_part1_bytes(),
        "the filesystem did not grow to the partition: {blocks} blocks of {block} B against {} B",
        canvas.grown_part1_bytes()
    );

    // 成功收尾会删记录：还留着记录说明收尾没走完。目录刚交给了 root，非 root 的
    // `inspect` 打不开，先交还
    // SAFETY: geteuid / getegid 无参数、只读
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let owner = format!("{uid}:{gid}");
    let (code, _, chown_err) =
        common::sudo(&["find", state_path, "-type", "d", "-exec", "chown", &owner, "{}", "+"]);
    assert_eq!(code, 0, "cannot hand the record directories back: {chown_err}");
    let (code, out, inspect_err) = common::run(&state, &["inspect", path]);
    assert_eq!(code, 0, "inspect must succeed after a finished job: {inspect_err}");
    assert!(
        !out.contains("audit: placement="),
        "a finished job must leave no record behind: {out}"
    );
}
