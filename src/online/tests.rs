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

/// 三态分类：结论随"表写没写"这一事实走，原始错误信息原样透传
#[test]
pub(crate) fn part_resize_error_classification() {
    let cls = |e: &PartResizeError| classify_part_resize_error(e);
    assert!(matches!(cls(&PartResizeError::NoWrite(io::Error::other("spawn"))), ResizeFailClass::Infra(_)));
    assert!(matches!(cls(&PartResizeError::Unknown(io::Error::other("x"))), ResizeFailClass::Failed(_)));
    assert!(matches!(cls(&PartResizeError::Partial(io::Error::other("x"))), ResizeFailClass::AppliedStaleKernel(_)));

    // 错误详情透传（发生处打印的是它，分类不得吞掉或改写）
    match cls(&PartResizeError::Unknown(io::Error::other("sfdisk said no"))) {
        ResizeFailClass::Failed(m) => assert!(m.contains("sfdisk said no"), "{m}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}