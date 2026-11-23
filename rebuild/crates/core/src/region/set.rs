//! 单区间与有序区间集合。
//!
//! 集合的不变量：按起点升序、两两不相交且不相邻、不含空区间。
//! 任何构造路径都维持该不变量，因此包含、重叠与求差都只需线性扫描。

/// 区间构造失败的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionError {
    /// 起点大于终点。
    Inverted { start: u64, end: u64 },
}

/// 半开区间 `[start, end)`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Region {
    start: u64,
    end: u64,
}

impl Region {
    /// 构造区间；`start == end` 表示空区间。
    pub fn new(start: u64, end: u64) -> Result<Self, RegionError> {
        if start > end {
            return Err(RegionError::Inverted { start, end });
        }
        Ok(Self { start, end })
    }

    /// 长度为 `len`、起点为 `start` 的区间。
    pub fn sized(start: u64, len: u64) -> Result<Self, RegionError> {
        let end = start.checked_add(len).ok_or(RegionError::Inverted {
            start,
            end: u64::MAX,
        })?;
        Self::new(start, end)
    }

    pub fn start(self) -> u64 {
        self.start
    }

    pub fn end(self) -> u64 {
        self.end
    }

    pub fn len(self) -> u64 {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub fn contains_offset(self, offset: u64) -> bool {
        self.start <= offset && offset < self.end
    }

    pub fn contains_region(self, other: Region) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    /// 两个非空区间有公共字节。
    pub fn overlaps(self, other: Region) -> bool {
        !self.is_empty() && !other.is_empty() && self.start < other.end && other.start < self.end
    }

    /// 交集；无公共字节时为 `None`。
    pub fn intersection(self, other: Region) -> Option<Region> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        if start < end {
            Some(Region { start, end })
        } else {
            None
        }
    }
}

/// 有序、互不相邻的区间集合。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegionSet {
    regions: Vec<Region>,
}

impl RegionSet {
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
        }
    }

    /// 由任意区间序列构造，自动排序、合并相邻区间并丢弃空区间。
    pub fn from_regions(regions: impl IntoIterator<Item = Region>) -> Self {
        let mut set = Self::new();
        for region in regions {
            set.insert(region);
        }
        set
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = Region> + '_ {
        self.regions.iter().copied()
    }

    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    /// 所有区间长度之和；溢出返回 `None`。
    pub fn total_len(&self) -> Option<u64> {
        self.regions
            .iter()
            .try_fold(0u64, |acc, region| acc.checked_add(region.len()))
    }

    /// 集合覆盖某个偏移。
    pub fn contains_offset(&self, offset: u64) -> bool {
        self.regions
            .binary_search_by(|region| {
                if region.end <= offset {
                    std::cmp::Ordering::Less
                } else if region.start > offset {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    /// 集合完整覆盖某个非空区间。
    pub fn covers(&self, region: Region) -> bool {
        if region.is_empty() {
            return true;
        }
        self.regions
            .iter()
            .any(|member| member.contains_region(region))
    }

    /// 与给定区间相交的部分。
    pub fn overlaps(&self, region: Region) -> bool {
        self.regions.iter().any(|member| member.overlaps(region))
    }

    /// 插入区间，必要时与相邻区间合并。
    pub fn insert(&mut self, region: Region) {
        if region.is_empty() {
            return;
        }
        let mut all = std::mem::take(&mut self.regions);
        all.push(region);
        all.sort_unstable_by_key(|member| (member.start, member.end));
        let mut merged: Vec<Region> = Vec::with_capacity(all.len());
        for member in all {
            if let Some(last) = merged.last_mut() {
                if member.start <= last.end {
                    last.end = last.end.max(member.end);
                    continue;
                }
            }
            merged.push(member);
        }
        self.regions = merged;
    }

    /// 从集合中移除一段区间，必要时把一个区间切成两段。
    pub fn remove(&mut self, region: Region) {
        if region.is_empty() {
            return;
        }
        let mut kept = Vec::with_capacity(self.regions.len() + 1);
        for member in self.regions.drain(..) {
            let Some(overlap) = member.intersection(region) else {
                kept.push(member);
                continue;
            };
            if member.start < overlap.start {
                kept.push(Region {
                    start: member.start,
                    end: overlap.start,
                });
            }
            if overlap.end < member.end {
                kept.push(Region {
                    start: overlap.end,
                    end: member.end,
                });
            }
        }
        self.regions = kept;
    }

    /// 只保留落在给定区间内的部分。
    pub fn intersect(&mut self, region: Region) {
        self.regions = self
            .regions
            .drain(..)
            .filter_map(|member| member.intersection(region))
            .collect();
    }

    /// 两个集合的交集。
    pub fn intersection(&self, other: &RegionSet) -> RegionSet {
        let mut result = RegionSet::new();
        for left in &self.regions {
            for right in &other.regions {
                if let Some(region) = left.intersection(*right) {
                    result.insert(region);
                }
            }
        }
        result
    }

    /// 两个集合的并集。
    pub fn union(&self, other: &RegionSet) -> RegionSet {
        let mut result = self.clone();
        for region in other.iter() {
            result.insert(region);
        }
        result
    }

    /// 差集 `self - other`。
    pub fn difference(&self, other: &RegionSet) -> RegionSet {
        let mut result = self.clone();
        for region in other.iter() {
            result.remove(region);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(start: u64, end: u64) -> Region {
        Region::new(start, end).expect("测试区间应合法")
    }

    fn set(ranges: &[(u64, u64)]) -> RegionSet {
        RegionSet::from_regions(ranges.iter().map(|&(start, end)| region(start, end)))
    }

    #[test]
    fn region_rejects_inverted_bounds() {
        assert_eq!(
            Region::new(8, 4),
            Err(RegionError::Inverted { start: 8, end: 4 })
        );
    }

    #[test]
    fn empty_region_bounds() {
        let empty = region(7, 7);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert!(!empty.contains_offset(7));
        assert!(!empty.overlaps(region(0, 100)));
        assert_eq!(empty.intersection(region(0, 100)), None);
    }

    #[test]
    fn maximum_bounds_do_not_overflow() {
        let top = region(u64::MAX - 1, u64::MAX);
        assert_eq!(top.len(), 1);
        assert!(top.contains_offset(u64::MAX - 1));
        assert_eq!(Region::sized(u64::MAX, 1).is_err(), true);
    }

    #[test]
    fn contains_and_overlaps_are_half_open() {
        let r = region(10, 20);
        assert!(r.contains_offset(10));
        assert!(!r.contains_offset(20));
        assert!(!r.overlaps(region(20, 30)));
        assert!(r.overlaps(region(19, 21)));
        assert_eq!(r.intersection(region(15, 25)), Some(region(15, 20)));
    }

    #[test]
    fn insert_merges_adjacent_and_overlapping() {
        let built = set(&[(20, 30), (0, 5), (5, 10), (25, 40)]);
        assert_eq!(built.regions(), &[region(0, 10), region(20, 40)]);
        assert_eq!(built.len(), 2);
        assert_eq!(built.total_len(), Some(30));
    }

    #[test]
    fn insert_discards_empty() {
        let built = set(&[(4, 4), (10, 12)]);
        assert_eq!(built.regions(), &[region(10, 12)]);
    }

    #[test]
    fn remove_splits_member() {
        let mut built = set(&[(0, 100)]);
        built.remove(region(40, 60));
        assert_eq!(built.regions(), &[region(0, 40), region(60, 100)]);
    }

    #[test]
    fn remove_clips_edges() {
        let mut built = set(&[(10, 20), (30, 40)]);
        built.remove(region(0, 35));
        assert_eq!(built.regions(), &[region(35, 40)]);
    }

    #[test]
    fn intersect_keeps_inner_part() {
        let mut built = set(&[(0, 10), (20, 30), (40, 50)]);
        built.intersect(region(5, 45));
        assert_eq!(
            built.regions(),
            &[region(5, 10), region(20, 30), region(40, 45)]
        );
    }

    #[test]
    fn covers_requires_single_member() {
        let built = set(&[(0, 10), (20, 30)]);
        assert!(built.covers(region(2, 8)));
        assert!(!built.covers(region(0, 30)));
        assert!(built.covers(region(5, 5)));
    }

    #[test]
    fn set_operations_match_ranges() {
        let left = set(&[(0, 10), (20, 30)]);
        let right = set(&[(5, 25), (40, 50)]);
        assert_eq!(
            left.intersection(&right).regions(),
            &[region(5, 10), region(20, 25)]
        );
        assert_eq!(
            left.union(&right).regions(),
            &[region(0, 30), region(40, 50)]
        );
        assert_eq!(
            left.difference(&right).regions(),
            &[region(0, 5), region(25, 30)]
        );
    }

    #[test]
    fn contains_offset_across_members() {
        let built = set(&[(0, 10), (10, 20), (30, 32)]);
        assert_eq!(built.regions(), &[region(0, 20), region(30, 32)]);
        for offset in 0..22 {
            assert_eq!(built.contains_offset(offset), offset < 20);
        }
    }

    #[test]
    fn total_len_covers_full_universe() {
        let built = set(&[(0, u64::MAX)]);
        assert_eq!(built.total_len(), Some(u64::MAX));
        assert_eq!(RegionSet::new().total_len(), Some(0));
    }
}
