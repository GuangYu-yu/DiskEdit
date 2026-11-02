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
/// 退出码 20，且不算完全成功。成因只改变文案，不改变出口契约
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
        let o = Outcome::applied_with(vec![Pending::new(1, PendingKind::Fs, msg.clone(), "")]);
        assert!(!o.is_complete(), "a skipped FS step must not read as complete: {msg}");
        assert_eq!(o.exit_code(), crate::outcome::EXIT_PARTIAL, "{msg}");
        match o {
            Outcome::Applied { pending, .. } => assert_eq!(pending.len(), 1, "{msg}"),
            other => panic!("expected Applied, got {other:?}"),
        }
    }
}

/// 分类：结论随"表写没写、写到什么程度"这一事实走，原始错误信息原样透传
#[test]
pub(crate) fn part_resize_error_classification() {
    let cls = |e: &PartResizeError| classify_part_resize_error(e);
    assert!(matches!(cls(&PartResizeError::NoWrite(io::Error::other("spawn"))), ResizeFailClass::Infra(_)));
    assert!(matches!(cls(&PartResizeError::Unknown(io::Error::other("x"))), ResizeFailClass::Failed(_)));
    assert!(matches!(cls(&PartResizeError::Unverified(io::Error::other("x"))), ResizeFailClass::WrittenUnverified(_)));
    assert!(matches!(cls(&PartResizeError::Partial(io::Error::other("x"))), ResizeFailClass::AppliedStaleKernel(_)));

    // 错误详情透传（发生处打印的是它，分类不得吞掉或改写）
    match cls(&PartResizeError::Unknown(io::Error::other("sfdisk said no"))) {
        ResizeFailClass::Failed(m) => assert!(m.contains("sfdisk said no"), "{m}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// 三种终态必须落在不同的出口上：成功（写入 + 盘上核验 + 内核同步都完成）→ 0；
/// 表已写且核验通过、仅内核同步失败 → 20 且内核视图置为过期；
/// 写入已发出但盘上状态不可证实 → 30，绝不能落进 Applied（那会声称"表已写"）
#[test]
pub(crate) fn part_resize_outcomes_keep_the_write_stage_distinct() {
    use crate::outcome::{EXIT_INFRA, EXIT_OK, EXIT_PARTIAL, KernelSync};

    let pending = |detail: &'static str| Pending::new(1, PendingKind::Fs, detail, "grow the filesystem manually");

    let ok = Outcome::applied_with(Vec::new());
    assert_eq!(ok.exit_code(), EXIT_OK);
    assert!(ok.is_complete());

    let o = PartResizeError::Partial(io::Error::other("partx failed")).into_outcome();
    assert_eq!(o.exit_code(), EXIT_PARTIAL);
    assert!(matches!(o, Outcome::Applied { kernel_sync: KernelSync::Stale, .. }));

    let o = PartResizeError::Unverified(io::Error::other("re-read failed"))
        .into_outcome_with(vec![pending("FS grow skipped")]);
    assert_eq!(o.exit_code(), EXIT_INFRA);
    match o {
        Outcome::Failed { cause, pending } => {
            assert!(cause.contains("re-read failed"), "{cause}");
            assert_eq!(pending.len(), 1);
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}