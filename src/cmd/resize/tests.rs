use super::resolve_size_request;

/// SIZE 请求解析：绝对/增量/百分比（先乘后除、下取整到 1MiB），锚定当前分区字节数
#[test]
pub(crate) fn size_request_resolution() {
    const MIB: u64 = 1024 * 1024;
    const G: u64 = 1024 * MIB;
    let r = |p: &str, cur: u64| resolve_size_request(Some(p), cur).unwrap();
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
    // "grow" 是唯一表示"吃满右侧空闲"的写法
    assert_eq!(r("grow", 1), (None, true));
    // 无 SIZE 也无 grow：显式拒绝
    assert!(resolve_size_request(None, 1).is_err());
}
