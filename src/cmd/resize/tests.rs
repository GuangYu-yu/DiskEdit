use super::resolve_size_request;

/// 写表前的 LVM 预检错误按成因分道：请求与卷现状不符给 3（改 `--lv` 有解），
/// 工具缺失给 6；只读查询失败给 4 但不带"盘可能已改变"——此刻确定一次目标写盘都没做
#[cfg(target_os = "linux")]
#[test]
pub(crate) fn lvm_preflight_errors_land_on_the_right_exit_channel() {
    use crate::lvm::LvmError;
    use crate::outcome::RunOutcome;

    let refused = super::lvm_preflight_fail(LvmError::Refused(
        "VG vg0: multiple LVs on /dev/sda2 — pick one with --lv NAME".into(),
    ));
    assert!(matches!(refused, RunOutcome::Refused(_)), "a mismatched request must be refused: {refused:?}");
    assert_eq!(refused.exit_code(), crate::outcome::EXIT_REFUSED);

    // 非活动 LV 同属"请求与现状对不上"：激活与否是系统运行态，改参数无解但也不是写盘前故障
    let inactive = super::lvm_preflight_fail(LvmError::Inactive(
        "LV /dev/vg0/lv1 is not active".into(),
    ));
    assert!(matches!(inactive, RunOutcome::Refused(_)), "an inactive LV must be refused: {inactive:?}");
    assert_eq!(inactive.exit_code(), crate::outcome::EXIT_REFUSED);

    let missing = super::lvm_preflight_fail(LvmError::ToolMissing("pvresize not found".into()));
    assert!(matches!(missing, RunOutcome::Unsupported { .. }), "a missing tool must be unsupported: {missing:?}");
    assert_eq!(missing.exit_code(), crate::outcome::EXIT_UNSUPPORTED);

    let failed = super::lvm_preflight_fail(LvmError::Failed("lvs failed: exit status 5".into()));
    assert!(matches!(failed, RunOutcome::NoMutation { .. }), "an environment failure before any write is NoMutation: {failed:?}");
    assert_eq!(failed.exit_code(), crate::outcome::EXIT_NO_MUTATION);
}

/// SIZE 请求解析：绝对/增量/百分比（先乘后除、下取整到 1MiB），锚定当前分区字节数
#[test]
pub(crate) fn size_request_resolution() {
    const MIB: u64 = 1024 * 1024;
    const G: u64 = 1024 * MIB;
    let r = |s: crate::cli::SizeSpec, cur: u64| resolve_size_request(s, cur).unwrap();
    assert_eq!(r(crate::cli::SizeSpec::Absolute(10 * G), 1), (Some(10 * G), false));
    assert_eq!(
        r(crate::cli::SizeSpec::Delta { grow: true, bytes: 2 * G }, G),
        (Some(3 * G), false)
    );
    assert_eq!(
        r(crate::cli::SizeSpec::Delta { grow: false, bytes: 500 * MIB }, G),
        (Some(G - 500 * MIB), false)
    );
    // 百分比：raw = cur×10/100 后向下取整到 1MiB
    let raw = G * 10 / 100; // 107374182
    let delta = raw / MIB * MIB; // 106954752
    let ten = crate::cli::SizeSpec::Percent { grow: true, num: 10, denom: 1 };
    assert_eq!(r(ten, G), (Some(G + delta), false));
    assert_eq!(
        r(crate::cli::SizeSpec::Percent { grow: false, num: 10, denom: 1 }, G),
        (Some(G - delta), false)
    );
    // 百分比增量不足 1MiB 时取整为 0（近似语义）
    assert_eq!(
        r(crate::cli::SizeSpec::Percent { grow: true, num: 1, denom: 1 }, 5 * MIB),
        (Some(5 * MIB), false)
    );
    // "grow" 是唯一表示"吃满右侧空闲"的写法
    assert_eq!(r(crate::cli::SizeSpec::Grow, 1), (None, true));
}
