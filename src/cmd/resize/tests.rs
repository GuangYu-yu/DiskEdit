use super::{resolve_size_request, SizeRequest};

/// SIZE 请求解析：绝对/增量/百分比（先乘后除、下取整到 1MiB），锚定当前分区字节数
#[test]
pub(crate) fn size_request_resolution() {
    const MIB: u64 = 1024 * 1024;
    const G: u64 = 1024 * MIB;
    let pos = |p: &str| SizeRequest { pos: Some(p.to_string()), abs: None, grow_to_end: false };
    let r = |p: &str, cur: u64| resolve_size_request(&pos(p), cur).unwrap();
    assert_eq!(r("10G", 1), (Some(10 * G), false));
    assert_eq!(r("+2G", G), (Some(3 * G), false));
    assert_eq!(r("-500M", G), (Some(G - 500 * MIB), false));
    // 百分比：raw = cur×10/100 后向下取整到 1MiB
    let raw = G * 10 / 100; // 107374182
    let delta = raw / MIB * MIB; // 106954752
    assert_eq!(r("+10%", G), (Some(G + delta), false));
    assert_eq!(r("-10%", G), (Some(G - delta), false));
    // 百分比增量不足 1MiB 时取整为 0（近似语义）
    assert_eq!(r("+1%", 5 * MIB), (Some(5 * MIB), false));
    // "grow" 与 --grow-to-end 等价
    assert_eq!(r("grow", 1), (None, true));
    // --size 走绝对目标；--grow-to-end 走 grow 标记
    let abs = SizeRequest { pos: None, abs: Some(4096), grow_to_end: false };
    assert_eq!(resolve_size_request(&abs, 1).unwrap(), (Some(4096), false));
    let grow = SizeRequest { pos: None, abs: None, grow_to_end: true };
    assert_eq!(resolve_size_request(&grow, 1).unwrap(), (None, true));
    // 既无 SIZE 也无 grow 标记：显式拒绝
    assert!(resolve_size_request(&SizeRequest::default(), 1).is_err());
}