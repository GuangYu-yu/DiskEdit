use super::*;

#[test]
pub(crate) fn parse_sysfs_u64_trims() {
    assert_eq!(parse_sysfs_u64("4096\n"), Some(4096));
    assert_eq!(parse_sysfs_u64("  3\n"), Some(3));
    assert_eq!(parse_sysfs_u64("x\n"), None);
}

#[test]
pub(crate) fn disk_name_strip_variants() {
    let exists = |n: &str| matches!(n, "nvme0n1" | "sda" | "loop0" | "mmcblk0");
    assert_eq!(disk_name_from_partition("nvme0n1p3", exists), Some("nvme0n1".into()));
    assert_eq!(disk_name_from_partition("sda1", exists), Some("sda".into()));
    assert_eq!(disk_name_from_partition("loop0p1", exists), Some("loop0".into()));
    assert_eq!(disk_name_from_partition("mmcblk0p2", exists), Some("mmcblk0".into()));
    // 盘名不存在 → None（防误切）
    assert_eq!(disk_name_from_partition("sda9", |_n| false), None);
    // 纯数字目录名不是分区
    assert_eq!(disk_name_from_partition("123", exists), None);
}

#[test]
pub(crate) fn dev_attr_parsing() {
    assert_eq!(parse_dev_attr("8:1\n"), Some((8, 1)));
    assert_eq!(parse_dev_attr(" 259:0 "), Some((259, 0)));
    assert_eq!(parse_dev_attr("8"), None);
    assert_eq!(parse_dev_attr("sda"), None);
    assert_eq!(parse_dev_attr("8:"), None);
}

#[test]
pub(crate) fn range_free_detects_overlap() {
    let others = vec![(2048u64, 4096u64)]; // [2048, 6144)
    assert!(range_free(6144, 512, &others));
    assert!(range_free(0, 2048, &others));
    assert!(!range_free(6000, 512, &others));
    assert!(!range_free(1024, 4096, &others));
    assert!(!range_free(6100, 100, &others));
}

#[test]
pub(crate) fn range_free_overflow_is_rejection() {
    // 既有区间终点 s+l 溢出：视为重叠，不得通过
    let others = vec![(u64::MAX - 100, 1000u64)];
    assert!(!range_free(u64::MAX - 50, 10, &others));
    // 新区间自身溢出同样拒绝
    assert!(!range_free(u64::MAX - 10, 100, &[]));
}

/// `/proc/swaps` 的设备路径反解不出来时必须上抛，不能折叠成"未占用"——那会让活动
/// swap 的锁下复核被一次条目解析失败绕过。挂载点表的 pseudo-fs 条目（source 不是
/// 路径）仍按不命中处理，两种来源的"目标不在"语义不同
#[test]
pub(crate) fn part_in_use_does_not_fold_a_missing_swap_source_into_absent() {
    let missing = "/nonexistent/diskedit_part_in_use";
    let e = part_in_use(missing, "sda", 1, false).expect_err("a swap entry that cannot be resolved must be an error");
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
    assert!(!part_in_use(missing, "sda", 1, true).expect("a mountinfo pseudo-fs source may be absent"));
}

/// 构造的 `/proc/swaps`：只有表头 ⇒ 明确"无 swap"；非块设备的 pseudo 条目
/// （/dev/null）不是目标盘的分区，也不报错
#[test]
pub(crate) fn swap_table_without_resolvable_entries_is_not_active() {
    use super::guard::swap_active_in;
    let header = "Filename\tType\tSize\tUsed\tPriority\n";
    assert!(!swap_active_in(header, "sda", 1).expect("a header-only table means no active swap"));
    assert!(
        !swap_active_in(&format!("{header}/dev/null\tfile\t0\t0\t-2\n"), "sda", 1)
            .expect("a non-block swap source cannot be a partition of the target")
    );
}

/// 条目无法反解 ⇒ Err，错误点明是哪一条、并说明目标状态无法确认；
/// 不能退化成 `Ok(false)`（fail-open），也不能说成"目标确实在 swap 上"（误判）
#[test]
pub(crate) fn unparsable_swap_entry_is_an_error_not_absent() {
    use super::guard::swap_active_in;
    let swaps =
        "Filename\tType\tSize\tUsed\tPriority\n/nonexistent/diskedit_swap_entry\tpartition\t0\t0\t-2\n";
    let e = swap_active_in(swaps, "sda", 1).expect_err("an unresolvable swap entry must not be folded into \"absent\"");
    let msg = e.to_string();
    assert!(msg.contains("/nonexistent/diskedit_swap_entry"), "{msg}");
    assert!(msg.contains("swap state cannot be confirmed"), "{msg}");
    assert!(!msg.contains("is active swap"), "the message must not claim the target is in swap: {msg}");
}

/// 表已写之后 FS 步失败的三种成因各有自己的结论：一个进程都没起、进程没起来、
/// 进程跑过并带错误退出——最后一种可能已经把文件系统改了一半，不能与"没动"同句
#[test]
pub(crate) fn grow_failures_after_resize_state_what_was_touched() {
    use super::flow::fs_grow_skipped_after_resize;
    use crate::fsops::FsError;

    let no_tool = fs_grow_skipped_after_resize(&FsGrowError::NoTool("no online tool for X".into()));
    let spawn = fs_grow_skipped_after_resize(&FsGrowError::Spawn(FsError::ToolMissing("resize2fs".into())));
    let exit = fs_grow_skipped_after_resize(&FsGrowError::Exit("resize2fs exited 1".into()));

    assert!(no_tool.contains("no filesystem grow tool was available"), "{no_tool}");
    assert!(spawn.contains("could not be started"), "{spawn}");
    assert!(exit.contains("may be partially grown"), "{exit}");
    // 原始错误正文必须随行（三种成因的细节都只在它里面）
    assert!(no_tool.contains("no online tool for X"), "{no_tool}");
    assert!(spawn.contains("resize2fs"), "{spawn}");
    assert!(exit.contains("resize2fs exited 1"), "{exit}");
    assert_ne!(no_tool, spawn);
    assert_ne!(spawn, exit);
    assert_ne!(no_tool, exit);
}

/// 表已写之后 FS 步失败的三种成因都只能是"未满足的后置条件"：结果仍是 Applied，
/// 退出码 5，且不算完全成功。成因只改变文案，不改变出口语义
#[test]
pub(crate) fn grown_partition_with_skipped_fs_step_stays_partial() {
    use super::flow::fs_grow_skipped_after_resize;
    use crate::fsops::FsError;

    let failures = [
        FsGrowError::NoTool("no online tool for X".into()),
        FsGrowError::Spawn(FsError::ToolMissing("resize2fs".into())),
        FsGrowError::Exit("resize2fs exited 1".into()),
    ];
    for e in failures {
        let msg = fs_grow_skipped_after_resize(&e);
        let o = RunOutcome::applied_with(vec![Pending::new(1, PendingKind::Fs, msg.clone(), "")]);
        assert!(!o.is_complete(), "a skipped FS step must not read as complete: {msg}");
        assert_eq!(o.exit_code(), crate::outcome::EXIT_PARTIAL, "{msg}");
        match o {
            RunOutcome::Applied { pending, .. } => assert_eq!(pending.len(), 1, "{msg}"),
            other => panic!("expected Applied, got {other:?}"),
        }
    }
}

/// 分类：结论随"表写没写、写到什么程度"这一事实走，原始错误信息原样透传
#[test]
pub(crate) fn part_resize_error_classification() {
    let cls = |e: &PartResizeError| classify_part_resize_error(e);
    assert!(matches!(cls(&PartResizeError::NoWrite(io::Error::other("spawn"))), ResizeFailClass::NoMutation(_)));
    assert!(matches!(cls(&PartResizeError::Unknown(io::Error::other("x"))), ResizeFailClass::Partial(_)));
    assert!(matches!(cls(&PartResizeError::Unverified(io::Error::other("x"))), ResizeFailClass::WrittenUnverified(_)));
    assert!(matches!(cls(&PartResizeError::Partial(io::Error::other("x"))), ResizeFailClass::AppliedStaleKernel(_)));

    // 错误详情透传（发生处打印的是它，分类不得吞掉或改写）
    match cls(&PartResizeError::Unknown(io::Error::other("sfdisk said no"))) {
        ResizeFailClass::Partial(m) => assert!(m.contains("sfdisk said no"), "{m}"),
        other => panic!("expected Partial, got {other:?}"),
    }
}

/// 三种终态必须落在不同的出口上：成功（写入 + 盘上核验 + 内核同步都完成）→ 0；
/// 表已写且核验通过、仅内核同步失败 → 5 且内核视图置为过期；
/// 写入已发出但盘上状态不可证实 → 5，绝不能落进 Applied（那会声称"表已写"）
#[test]
pub(crate) fn part_resize_outcomes_keep_the_write_stage_distinct() {
    use crate::outcome::{EXIT_OK, EXIT_PARTIAL, KernelSync};

    let pending = |detail: &'static str| Pending::new(1, PendingKind::Fs, detail, "grow the filesystem manually");

    let ok = RunOutcome::applied_with(Vec::new());
    assert_eq!(ok.exit_code(), EXIT_OK);
    assert!(ok.is_complete());

    let o = PartResizeError::Partial(io::Error::other("partx failed")).into_run_outcome();
    assert_eq!(o.exit_code(), EXIT_PARTIAL);
    assert!(matches!(o, RunOutcome::Applied { kernel_sync: KernelSync::Stale, .. }));

    let o = PartResizeError::Unverified(io::Error::other("re-read failed"))
        .into_run_outcome_with(vec![pending("FS grow skipped")]);
    assert_eq!(o.exit_code(), EXIT_PARTIAL);
    match o {
        RunOutcome::Partial { cause, pending } => {
            assert!(cause.contains("re-read failed"), "{cause}");
            assert_eq!(pending.len(), 1);
        }
        other => panic!("expected Partial, got {other:?}"),
    }
}

// ---- 真实块设备回归（Linux + root + loop + btrfs）----
//
// 在线缩容的首个写盘步骤是 `btrfs filesystem resize`。决策快照取自内核视图，之后到
// 首写之间存在探测窗口；本用例在 `decide`（锁下、快照之后）里改写盘上表，模拟窗口内
// 的第三方改动。判据：命令必须以 3 拒绝，且镜像（loop 的 backing 文件）与改动后的
// 盘面逐字节一致——若 FS 步先于写表复核执行，字节已被改小。
//
// 不满足环境时直接说明跳过：这台机上没有能覆盖该层的替代物（写表复核本身另有单测，
// 但顺序缺陷只有端到端才可见）。

#[cfg(target_os = "linux")]
struct OnlineSandbox {
    img: String,
    mnt: String,
    loop_dev: Option<String>,
}

#[cfg(target_os = "linux")]
impl Drop for OnlineSandbox {
    fn drop(&mut self) {
        drop(std::process::Command::new("umount").args(["-l", &self.mnt]).status());
        if let Some(d) = &self.loop_dev {
            drop(std::process::Command::new("losetup").args(["-d", d]).status());
        }
        drop(std::process::Command::new("rm").args(["-rf", &self.mnt]).status());
        drop(std::process::Command::new("rm").args(["-f", &self.img]).status());
        drop(
            std::process::Command::new("sh")
                .args(["-c", &format!("rm -f {0}.diskedit.lock {0}.diskedit.journal", self.img)])
                .status(),
        );
    }
}

#[cfg(target_os = "linux")]
fn sh_ok(script: &str) -> String {
    let out = std::process::Command::new("sh")
        .args(["-c", script])
        .output()
        .unwrap_or_else(|e| panic!("cannot run shell: {e}"));
    assert!(
        out.status.success(),
        "command failed: {script}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[cfg(target_os = "linux")]
fn sha256_file(path: &str) -> String {
    let out = sh_ok(&format!("sha256sum {path}"));
    out.split_whitespace().next().expect("sha256sum output").to_string()
}

#[cfg(target_os = "linux")]
#[test]
fn shrink_recheck_refuses_table_moved_before_first_write() {
    // 环境闸：root / 所需工具 / btrfs 内核支持；缺一项这条就无法在本机覆盖该层
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skip: online shrink recheck regression needs root (loop + mount + btrfs)");
        return;
    }
    for t in ["sfdisk", "mkfs.btrfs", "losetup", "mount", "sha256sum"] {
        if !std::process::Command::new("sh")
            .args(["-c", &format!("command -v {t} >/dev/null")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            eprintln!("skip: online shrink recheck regression needs {t}");
            return;
        }
    }
    if !std::path::Path::new("/sys/fs/btrfs").exists() {
        eprintln!("skip: online shrink recheck regression needs btrfs in the running kernel");
        return;
    }

    let img = "/tmp/diskedit-online-recheck.img";
    let mnt = "/tmp/diskedit-online-recheck-mnt";
    let mut sb = OnlineSandbox { img: img.into(), mnt: mnt.into(), loop_dev: None };
    drop(
        std::process::Command::new("sh")
            .args(["-c", &format!("umount -l {mnt} 2>/dev/null; rm -rf {mnt}; rm -f {img}")])
            .status(),
    );
    // 1G 镜像、1MiB 起 400MiB 的 GPT 分区：右移 1MiB 后仍有空间，且长度不变——
    // 长度不变才不会在 flow 的 `disk_len` 一致检查处提前出局
    sh_ok(&format!("truncate -s 1G {img} && printf 'label: gpt\\n,400M,L\\n' | sfdisk {img}"));
    let dev = sh_ok(&format!("losetup -fP --show {img}"));
    sb.loop_dev = Some(dev.clone());
    sh_ok(&format!("mkfs.btrfs -q -f {dev}p1"));
    sh_ok(&format!("mkdir -p {mnt} && mount {dev}p1 {mnt}"));

    let mut after_tamper = None;
    let outcome = crate::online::api::resize_online_planned(std::path::Path::new(mnt), |disk| {
        let (start, len) = crate::gpt_policy::partition_bytes(disk, 1)?;
        // 决策已给出（快照来自内核视图），此刻改写盘上表里本分区的起点，长度保持不变
        let moved = start + 1024 * 1024;
        let script = format!("printf '{},{}\\n' | sfdisk --force --no-reread -N 1 {dev}", moved / 512, len / 512);
        sh_ok(&script);
        after_tamper = Some(sha256_file(img));
        Ok(Some(len - 100 * 1024 * 1024))
    });

    assert_eq!(outcome.exit_code(), 3, "首写前发现盘上表已变必须拒绝，且未写盘: {outcome:?}");
    let after = sha256_file(img);
    let tampered = after_tamper.expect("the decide hook must have tampered the table");
    assert_eq!(after, tampered, "拒绝时镜像必须与改写后的盘面逐字节一致（整镜像 sha256）");
}
