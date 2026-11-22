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
    let r = |s: crate::cli::SizeSpec, cur: u64| resolve_size_request(s, cur, 512).unwrap();
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
    let raw = G * 10 / 100;
    let delta = raw / MIB * MIB;
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
    // 不足一个扇区的目标在解析处拒绝，锁前快照与锁下重算同判
    assert!(resolve_size_request(crate::cli::SizeSpec::Absolute(511), G, 512).is_err());
    assert!(resolve_size_request(crate::cli::SizeSpec::Delta { grow: false, bytes: G }, G, 512).is_err());
}

/// 身份闸接线：锁前快照 A、锁下同号条目 B（GUID/起点/容量至少一项不同）必须落 Refused
/// （退出 3），全等才放行。助手只接收条目值、不持盘句柄，拒绝分支不可能伴随已发生的
/// 写入；被拦的接线错误包括：比对字段集改漏、不符分支被翻转成放行、拒绝被折成别的出口。
#[test]
fn entry_identity_gate_refuses_reused_slot_before_any_write() {
    use crate::outcome::{EXIT_REFUSED, RunOutcome};

    let gpt_entry = |guid: [u8; 16], start: u64, end: u64| gptman::GPTPartitionEntry {
        partition_type_guid: [1; 16],
        unique_partition_guid: guid,
        starting_lba: start,
        ending_lba: end,
        attribute_bits: 0,
        partition_name: "".into(),
    };
    let snap_g = gpt_entry([2; 16], 2048, 4095);
    let other_g = gpt_entry([9; 16], 10240, 12000);
    let refused_g = super::gpt_identity_or_refused(&snap_g, &other_g, 1).unwrap_err();
    assert!(matches!(refused_g, RunOutcome::Refused(_)), "a reused slot must be refused: {refused_g:?}");
    assert_eq!(refused_g.exit_code(), EXIT_REFUSED);
    assert!(super::gpt_identity_or_refused(&snap_g, &gpt_entry([2; 16], 2048, 4095), 1).is_ok());

    let mbr_entry = |start: u32, size: u32| crate::table::MbrPartition {
        num: 1,
        os_type: 0x83,
        start_lba: start,
        size_lba: size,
        is_container: false,
    };
    let snap_m = mbr_entry(63, 100);
    let refused_m = super::mbr_identity_or_refused(&snap_m, &mbr_entry(200, 100), 1).unwrap_err();
    assert!(matches!(refused_m, RunOutcome::Refused(_)), "a reused slot must be refused: {refused_m:?}");
    assert_eq!(refused_m.exit_code(), EXIT_REFUSED);
    assert!(super::mbr_identity_or_refused(&snap_m, &mbr_entry(63, 100), 1).is_ok());
}
