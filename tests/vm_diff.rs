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
//! 物只有磁盘字节。记录里与放置有关的只有审计尾段，而它没有判定读者
//! （`tests/architecture_audit_tail.rs` 把读取面收成一个登记过的名字），所以"两条腿写的
//! 记录能不能互换"不是本文件的题目：那由 `tests/vm_interrupt.rs` 用一次真实的中断接手
//! 来回答。本文件只回答"两条腿的最终结果相同"。
//!
//! 前置：QEMU、真实 bundle、非 root 身份、免密 `sudo`。缺一即打印补验方式后跳过；
//! `DISKEDIT_REQUIRE_CAPABILITIES` 置位时跳过即失败。

#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

fn spawn_native(dir: &Path, image: &Path, state: &Path) -> Child {
    let out = std::fs::File::create(dir.join("native.stdout")).expect("create the log");
    let err = std::fs::File::create(dir.join("native.stderr")).expect("create the log");
    let args = common::resize_args(image);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    common::root_command(state, &args)
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
    let canvas = common::Canvas::new("vm_diff");
    let carrier = canvas.duplicate("carrier");
    let native = canvas.duplicate("native");
    let carrier_state = canvas.private_state("carrier");
    let native_state = canvas.private_state("native");

    // 载体腿：非 root、有 bundle ⇒ 宿主能力不足，作业交给载体。QEMU 子进程是放置的正面证据
    let out_log = std::fs::File::create(canvas.dir.join("carrier.stdout")).expect("create the log");
    let err_log = std::fs::File::create(canvas.dir.join("carrier.stderr")).expect("create the log");
    let args = common::resize_args(&carrier);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut carrier_leg = common::disk_edit()
        .args(&args)
        .env("DISKEDIT_STATE_DIR", &carrier_state)
        .stdout(Stdio::from(out_log))
        .stderr(Stdio::from(err_log))
        .spawn()
        .expect("spawn DiskEdit");
    let pid = carrier_leg.id();
    assert!(
        common::wait_until(Duration::from_secs(120), || common::qemu_child_of(pid).is_some()),
        "the non-root leg never started a carrier"
    );
    let status = carrier_leg.wait().expect("the carrier leg ends");
    let carrier_err = std::fs::read_to_string(canvas.dir.join("carrier.stderr")).unwrap_or_default();
    assert_eq!(status.code().unwrap_or(-1), 0, "the carrier leg must finish: {carrier_err}");

    // 原生腿：root、有工具 ⇒ 宿主能力齐备，作业在宿主执行。整轮不得出现载体
    let mut native_leg = spawn_native(&canvas.dir, &native, &native_state);
    let mut saw_carrier = false;
    let native_status = loop {
        saw_carrier |= common::qemu_child_of(native_leg.id()).is_some();
        if let Some(status) = native_leg.try_wait().expect("query the native leg") {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let native_err = std::fs::read_to_string(canvas.dir.join("native.stderr")).unwrap_or_default();
    assert!(!saw_carrier, "the root leg started a carrier: the host could do the job itself");
    assert_eq!(native_status.code().unwrap_or(-1), 0, "the native leg must finish: {native_err}");

    let size = std::fs::metadata(&carrier).expect("carrier image").len();
    assert_eq!(
        std::fs::metadata(&native).expect("native image").len(),
        size,
        "the two runs must start and end on the same canvas"
    );

    // 分界线之外：主备 GPT、分区 2 的字节必须逐字节相同
    assert_eq!(
        common::first_difference(&canvas.head(&carrier), &canvas.head(&native), 0),
        None,
        "the two placements disagree on the bytes before the partition"
    );
    assert_eq!(
        common::first_difference(
            &canvas.tail(&carrier, size),
            &canvas.tail(&native, size),
            canvas.tail_start()
        ),
        None,
        "the two placements disagree on the bytes after the partition"
    );

    // 分界线之内：FS 几何相同，且两条腿都把 FS 扩到了分区末尾
    let (block_a, blocks_a) = canvas.geometry(&carrier);
    let (block_b, blocks_b) = canvas.geometry(&native);
    assert_eq!(
        (block_a, blocks_a),
        (block_b, blocks_b),
        "the two placements disagree on the filesystem geometry"
    );
    assert_eq!(
        block_a * blocks_a,
        canvas.part1_bytes(),
        "the filesystem did not grow to fill the partition: {blocks_a} blocks of {block_a} B \
         against a partition of {} B",
        canvas.part1_bytes()
    );
}
