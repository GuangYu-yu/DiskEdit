//! 跨执行器接手：载体把记录停在"表已写、FS 未扩"，原生把它收完。
//!
//! 这是三层判据覆盖不到的唯一空档——那个中间态的盘面只有真实执行才产生：
//!
//! - 结构层（`src/recovery/record.rs::every_record_field_is_named_here`、
//!   `the_audit_tail_does_not_move_a_judgement`、`tests/architecture_audit_tail.rs`）：
//!   记录里没有随放置变动的判定输入；
//! - 决策层（`place_on_disk_reads_the_recorded_ranges`、
//!   `resume_place_separates_unbound_from_moved`）：盘面判定与"谁写的"无关；
//! - 结果层（`tests/vm_diff.rs`）：两条腿最终结果一致。
//!
//! 三层加起来仍不回答"这份**由载体写下**的记录，换个执行器还能不能接着做"。所以本文件
//! 只断言这一件事：原生重跑同一条命令，退出 0，并且只做了 FS 收尾。
//!
//! 停点用既有注入点，不靠信号抢时间窗：`test-faults` 构建下
//! `DISKEDIT_FAULT=after-table-write` 让宿主在"表步已写完、记录尚未推进"处 abort
//! （`src/movepart/faults.rs` 声明，`exec::dispatch::FaultPoint` 的语义就是"记录尚未推进"）。
//! 于是现场必然是：表已落、记录未推进、FS 未扩。
//!
//! 腿 1 为什么一定在载体上：`placement::needs_of` 取的是**整份作业**的能力并集，FS 步要求
//! root，非 root 身份下整份作业（含表步）交给载体。这条前提不靠假设——腿 1 结束后读记录
//! 的审计尾段，`audit: placement=Vm` 是它自证；若它说 `Native`，本用例测的就不是交接。
//!
//! **目标是镜像文件，不是块设备**，这一条是承载判据的：记录只由宿主写，两条腿必须共享
//! 同一个记录根，而镜像记录的根是每用户根、无条件认 `DISKEDIT_STATE_DIR`
//! （`dev::identity::image_state_root`）——非 root 的载体腿与 root 的原生腿因此指向同一处。
//! 块设备记录的根是 `dev::host_repo::host_state_root`，它在 `euid != 0` 时忽略这个变量，
//! 两条腿会各自落在一个根里，原生腿找不到载体写下的记录。块设备目标共用同一份记录与
//! 判定代码，但本用例不覆盖它：非 root 写不了块设备，root 又不会进载体，那个现场造不出来。
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

    // 腿 2：同一份镜像、同一条命令，root 身份 ⇒ 原生。整轮不得出现载体
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

    // 成功收尾会删记录：还留着记录说明收尾没走完
    let (code, out, inspect_err) = common::run(&state, &["inspect", path]);
    assert_eq!(code, 0, "inspect must succeed after a finished job: {inspect_err}");
    assert!(
        !out.contains("audit: placement="),
        "a finished job must leave no record behind: {out}"
    );
}
