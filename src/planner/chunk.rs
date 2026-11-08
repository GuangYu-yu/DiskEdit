//! 搬移的块布局算式：计划期与执行期共用同一处实现。

/// 搬移是否自重叠：位移小于源区间长度，即目的区间与源区间相交。
///
/// 判据只取几何，与"块大小是否已被收敛"无关：请求块本来就不大于位移时布局已安全，
/// 但重做仍会读到本批后续写覆盖过的源区，所以批量提交的开关必须按它分派。
/// 位移为 0 时有源区间与自己重合的语义，但没有任何拷贝要做，不算自重叠
pub(crate) fn relocation_overlaps(shift_lba: u64, src_len_lba: u64) -> bool {
    shift_lba != 0 && shift_lba < src_len_lba
}

/// 一条搬移的块布局：返回 `(每块字节数, 块数上界)`。
///
/// 逐块前移的覆盖顺序要求**自重叠**搬移（位移 < 源长度）的单次拷贝不超过位移，
/// 否则本次拷贝会写进自己还没读的源区。故有效块大小取 `chunk_bytes` 与位移字节的
/// 较小者。**唯一实现**：搬移循环、checkpoint 采纳与续跑校验都从这里取——同一算式
/// 抄在多处时，任何一处漂移都会变成"少拷却提交表项"或"重做读到已被覆盖的源"。
///
/// `copy_lba` 是本次要搬的扇区数（plan 条目 = 分区长度；move+shrink = 新旧区间的
/// 较小者），`src_len_lba` 是源区间长度，自重叠由 [`relocation_overlaps`] 判定。
/// 位移为 0 不算自重叠，块大小取请求值，返回 `Some((chunk_bytes, ceil(copy*ss/chunk)))`。
/// 有效块大小算成 0（请求块为 0，或自重叠且位移字节为 0）时返回 `None`，调用方按
/// 溢出/损坏拒绝，不拿 0 当除数
pub(crate) fn move_chunk_layout(
    copy_lba: u64,
    ss: u64,
    shift_lba: u64,
    src_len_lba: u64,
    chunk_bytes: u64,
) -> Option<(u64, u64)> {
    let chunk = if relocation_overlaps(shift_lba, src_len_lba) {
        chunk_bytes.min(shift_lba.checked_mul(ss)?)
    } else {
        chunk_bytes
    };
    if chunk == 0 {
        return None;
    }
    let count = copy_lba.checked_mul(ss)?.checked_add(chunk - 1)? / chunk;
    Some((chunk, count))
}
