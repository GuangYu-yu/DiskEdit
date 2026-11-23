//! 两个区间集合的差异。
//!
//! 差异用于写后核对：`removed` 是只在左侧出现的字节，`added` 是只在右侧出现的字节。
//! 两者都为空等价于两个集合逐字节相等。

use super::set::RegionSet;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegionDiff {
    added: RegionSet,
    removed: RegionSet,
}

impl RegionDiff {
    /// 计算 `right - left` 与 `left - right`。
    pub fn between(left: &RegionSet, right: &RegionSet) -> Self {
        Self {
            added: right.difference(left),
            removed: left.difference(right),
        }
    }

    pub fn added(&self) -> &RegionSet {
        &self.added
    }

    pub fn removed(&self) -> &RegionSet {
        &self.removed
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region::Region;

    fn region(start: u64, end: u64) -> Region {
        Region::new(start, end).expect("测试区间应合法")
    }

    fn set(ranges: &[(u64, u64)]) -> RegionSet {
        RegionSet::from_regions(ranges.iter().map(|&(start, end)| region(start, end)))
    }

    #[test]
    fn equal_sets_have_no_difference() {
        let left = set(&[(0, 10), (20, 30)]);
        let right = set(&[(0, 10), (20, 30)]);
        assert!(RegionDiff::between(&left, &right).is_empty());
    }

    #[test]
    fn reports_both_directions() {
        let left = set(&[(0, 10), (20, 30)]);
        let right = set(&[(5, 25)]);
        let diff = RegionDiff::between(&left, &right);
        assert_eq!(diff.added().regions(), &[region(10, 20)]);
        assert_eq!(diff.removed().regions(), &[region(0, 5), region(25, 30)]);
    }

    #[test]
    fn disjoint_sets_are_fully_added_and_removed() {
        let left = set(&[(0, 4)]);
        let right = set(&[(8, 12)]);
        let diff = RegionDiff::between(&left, &right);
        assert_eq!(diff.added().regions(), &[region(8, 12)]);
        assert_eq!(diff.removed().regions(), &[region(0, 4)]);
    }
}
