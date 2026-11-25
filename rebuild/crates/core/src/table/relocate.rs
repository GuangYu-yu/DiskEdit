//! 搬移与复制的块布局：分块大小、拷贝方向与续跑判定。
//!
//! 目标区间与源区间相交时，拷贝顺序决定正确性：位移为正（目标在右）必须尾→头，
//! 位移为负（目标在左）必须头→尾；自重叠时单块不得超过位移，否则本块会写进自己
//! 还没读的源区。这三条都由本模块唯一给出，判定、执行与续跑校验共用同一份算式。

/// 源区间里的一块（LBA）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkSpan {
    pub first_lba: u64,
    pub len_lba: u64,
}

/// 一次搬移/复制的块布局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// 单块扇区数，恒大于零。
    pub chunk_lba: u64,
    /// 要拷贝的源扇区数。
    pub copy_lba: u64,
    /// 块数。
    pub count: u64,
    /// 目标起点减源起点的有符号位移。
    pub shift: i64,
}

impl Layout {
    /// 按拷贝顺序返回源区间的块：位移为正时从尾到头，其余从头到尾。
    ///
    /// `source_lba` 是源区间的第一个 LBA；目标块由源块加 [`Self::shift`] 得到。
    pub fn spans(&self, source_lba: u64) -> Vec<ChunkSpan> {
        let mut spans = Vec::new();
        for index in 0..self.count {
            let offset = index * self.chunk_lba;
            let len = self.chunk_lba.min(self.copy_lba - offset);
            spans.push(ChunkSpan {
                first_lba: source_lba.saturating_add(offset),
                len_lba: len,
            });
        }
        if self.shift > 0 {
            spans.reverse();
        }
        spans
    }
}

/// 目标区间是否与源区间相交：位移非零且小于读取长度。
pub fn relocation_overlaps(shift_lba: u64, copy_lba: u64) -> bool {
    shift_lba != 0 && shift_lba < copy_lba
}

/// 由拷贝长度、扇区大小、位移与请求块字节数求出块布局。
///
/// 自重叠时单块收敛到不超过位移；结果块为零（请求块为零、或自重叠位移为零）
/// 或字节换算溢出时返回 `None`，调用方按参数或几何错误拒绝，不拿零当除数。
pub fn layout(copy_lba: u64, sector_size: u64, shift: i64, chunk_bytes: u64) -> Option<Layout> {
    if copy_lba == 0 {
        return Some(Layout {
            chunk_lba: 1,
            copy_lba,
            count: 0,
            shift,
        });
    }
    let magnitude = shift.unsigned_abs();
    let effective = if relocation_overlaps(magnitude, copy_lba) {
        chunk_bytes.min(magnitude.checked_mul(sector_size)?)
    } else {
        chunk_bytes
    };
    if effective == 0 || sector_size == 0 {
        return None;
    }
    let chunk_lba = effective / sector_size;
    if chunk_lba == 0 {
        return None;
    }
    Some(Layout {
        chunk_lba: chunk_lba.min(copy_lba),
        copy_lba,
        count: copy_lba.div_ceil(chunk_lba),
        shift,
    })
}

/// 续跑结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume {
    /// 没有可用的进度，从头开始。
    Start,
    /// 已写前缀相符，从该块游标继续。
    Continue { cursor: u64 },
    /// 记录里有进度，但目标前缀与之不符：不能安全续跑，也不得从头重放。
    Mismatch,
}

/// 由记录里的（块游标、已写前缀摘要）与重算的目标准前缀摘要决定能否续跑。
///
/// 游标为零或没有记录时从头开始；游标非零且摘要相符时继续；摘要不符是
/// [`Resume::Mismatch`]——自重叠搬移下从头重放会读到已被覆盖的源，必须拒绝。
pub fn resume(recorded: Option<(u64, [u8; 16])>, recomputed: [u8; 16]) -> Resume {
    match recorded {
        None => Resume::Start,
        Some((0, _)) => Resume::Start,
        Some((cursor, digest)) if digest == recomputed => Resume::Continue { cursor },
        Some(_) => Resume::Mismatch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SS: u64 = 512;

    #[test]
    fn a_forward_move_copies_tail_to_head() {
        let layout = layout(1000, SS, 2000, 1 << 20).expect("布局");
        assert_eq!(layout.count, 1);
        let spans = layout.spans(100);
        assert_eq!(
            spans,
            vec![ChunkSpan {
                first_lba: 100,
                len_lba: 1000
            }]
        );
    }

    #[test]
    fn a_right_shift_orders_chunks_from_the_tail() {
        // 1000 扇区按 250 分四块，位移为正且不相交：顺序应为最后一块在前。
        let layout = layout(1000, SS, 2000, 250 * SS).expect("布局");
        assert_eq!(layout.count, 4);
        let spans = layout.spans(0);
        assert_eq!(spans[0].first_lba, 750);
        assert_eq!(spans[3].first_lba, 0);
    }

    #[test]
    fn a_left_shift_orders_chunks_from_the_head() {
        let layout = layout(1000, SS, -2000, 250 * SS).expect("布局");
        let spans = layout.spans(2000);
        assert_eq!(spans[0].first_lba, 2000);
        assert_eq!(spans[3].first_lba, 2750);
    }

    #[test]
    fn self_overlap_clamps_the_chunk_to_the_shift() {
        // 位移 100 扇区，请求块 250 扇区：自重叠时单块收敛到 100。
        let layout = layout(1000, SS, 100, 250 * SS).expect("布局");
        assert_eq!(layout.chunk_lba, 100);
        assert_eq!(layout.count, 10);
    }

    #[test]
    fn a_non_overlapping_move_keeps_the_requested_chunk() {
        let layout = layout(1000, SS, 2000, 250 * SS).expect("布局");
        assert_eq!(layout.chunk_lba, 250);
        assert_eq!(layout.count, 4);
    }

    #[test]
    fn the_last_span_is_not_padded_past_the_copy_length() {
        let layout = layout(1001, SS, 100, 250 * SS).expect("布局");
        assert_eq!(layout.chunk_lba, 100);
        assert_eq!(layout.count, 11);
        let spans = layout.spans(0);
        // 位移为正时尾块在前：最后一块只有 1 个扇区，排在拷贝顺序的第一位。
        assert_eq!(spans[0].first_lba, 1000);
        assert_eq!(spans[0].len_lba, 1);
    }

    #[test]
    fn zero_geometry_is_rejected() {
        assert_eq!(layout(10, SS, 5, 0), None);
        assert_eq!(layout(10, 0, 5, 1024), None);
        assert!(layout(0, SS, 5, 1024).is_some());
    }

    #[test]
    fn b21_resume_checks_prefix_digest() {
        let digest = [0x5a; 16];
        assert_eq!(resume(None, digest), Resume::Start);
        assert_eq!(resume(Some((0, digest)), digest), Resume::Start);
        assert_eq!(
            resume(Some((3, digest)), digest),
            Resume::Continue { cursor: 3 }
        );
        assert_eq!(resume(Some((3, digest)), [0xff; 16]), Resume::Mismatch);
    }
}
