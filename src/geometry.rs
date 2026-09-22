//! GPT 全仓唯一的 **operational geometry** 类型。
//!
//! "条目数组有多大"是编解码/布局事实，canonical 类型 [`EntryArrayGeometry`] 与
//! 自定字节上限 [`MAX_ARRAY_BYTES`] 都定义在 codec 层（`table`），本模块从中引入
//! 消费（策略常量在构造点显式传入）。
//!
//! [`ValidatedGeometry`] 是"写操作的唯一入口事实"：它只由解析结果 + 修复动作构造，
//! 携带**修复完成后将成立**的几何（见 `new` 的 `post_repair_last_usable`）。写入路径只准
//! 消费它；`info` 一类诊断路径继续读 `table::RawGpt`——那是有意的能力保留：
//! 本工具拒绝在坏几何上继续操作，但必须能告诉用户哪里坏了。

use crate::table::{EntryArrayGeometry, MAX_ARRAY_BYTES, RawGpt, RawHeader};
use gptman::GPTPartitionEntry;
use std::io;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// checkpoint 之类"盘上可控的持久状态"的合法性判据来源：它们不自己发现世界，
/// 只消费已验证的几何（见 [`ValidatedGeometry::limits`]）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GeometryLimits {
    pub sector_size: u64,
    /// 分区号的上界（= 该表的条目数）。一个 256 槽位的表允许 1..=256
    pub entry_count: u32,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    /// 容器最后一个 LBA（本表 ss 口径）
    pub file_last_lba: u64,
}

/// 全仓唯一的"可操作几何事实"。构造即校验（含条目重叠），因此拿到它的代码不必再问
/// "这个几何可用吗"，也不必自己携带"条目互不重叠"的隐含前提。
///
/// `header` 内是**修复完成后**的值（`last_usable_lba` / `backup_lba` 已按修复动作收敛），
/// 于是 Planner 永远看不到 stale 几何，也不会出现"某处再算一次 effective_last_usable"
#[derive(Clone)]
pub struct ValidatedGeometry {
    /// 该表自身的扇区大小（条目 LBA 的单位）
    pub ss: u64,
    /// 修复后将生效的头（last_usable_lba / backup_lba 已收敛）
    pub header: RawHeader,
    pub entries: Vec<GPTPartitionEntry>,
    pub entry_geometry: EntryArrayGeometry,
    /// 容器最后一个 LBA（本表 ss 口径）：几何自洽性的上界，也是 checkpoint 的 LBA 上界
    pub file_last_lba: u64,
}

impl ValidatedGeometry {
    /// 从解析结果构造可操作几何。
    ///
    /// 调用前提（由解析层保证）：条目端点不倒挂、且都落在**当前**头的 usable 区间内；
    /// 本函数在此之上补两条解析层不做的判据：
    /// 1. 条目数组几何必须自洽（与解析层同一构造点，见 `EntryArrayGeometry::new`）
    /// 2. **已定义条目不得互相重叠**——UEFI 2.10 §5.3.1 GPT overview：
    ///    "Each defined partition must not overlap with any other defined partition."
    ///    本工具所有派生事实（右侧空闲、搬移打包、扩容终点）都以"不重叠"为前提，
    ///    故它必须在构造点被验过，而不是靠每个消费者自己防守
    ///
    /// `post_repair_last_usable`：修复动作给出的、修复后将生效的 last_usable_lba
    /// （`None` = 无搬迁动作，沿用头里的值）
    pub fn new(g: &RawGpt, file_last_lba: u64, post_repair_last_usable: Option<u64>) -> io::Result<Self> {
        let entry_geometry = EntryArrayGeometry::new(
            g.ss,
            g.header.size_of_partition_entry,
            g.header.number_of_partition_entries,
            MAX_ARRAY_BYTES,
        )?;
        if let Some((a, b)) = find_overlap(&g.entries) {
            return Err(invalid(format!(
                "GPT partition entries #{a} and #{b} overlap — this tool refuses to operate on a table whose \
                 derived facts (free space, relocation packing, grow target) would be undefined; \
                 fix the overlap with sfdisk/parted first"
            )));
        }
        let mut header = g.header.clone();
        if let Some(last_usable) = post_repair_last_usable {
            header.last_usable_lba = last_usable;
        }
        // 修复后备份头位于容器末端；无修复动作时盘上值已等于它，赋值是恒等变换
        header.backup_lba = file_last_lba;
        Ok(Self { ss: g.ss, header, entries: g.entries.clone(), entry_geometry, file_last_lba })
    }

    pub fn first_usable_lba(&self) -> u64 {
        self.header.first_usable_lba
    }

    /// 修复后将生效的可用区上界。任何调用点都不得再自行推导（历史问题：多处各算一次）
    pub fn last_usable_lba(&self) -> u64 {
        self.header.last_usable_lba
    }

    /// 1-based 分区号 → 条目下标；越界即 None（分区号上界来自本表的条目数，不是常量）
    pub fn entry_index(&self, part: u32) -> Option<usize> {
        self.entry_geometry.slot(part)
    }

    pub fn limits(&self) -> GeometryLimits {
        GeometryLimits {
            sector_size: self.ss,
            entry_count: self.entry_geometry.entry_count,
            first_usable_lba: self.header.first_usable_lba,
            last_usable_lba: self.header.last_usable_lba,
            file_last_lba: self.file_last_lba,
        }
    }

    /// 用本几何提交分区表。这是几何消费路径（搬移/扩容/修复）唯一的提交入口：
    /// 几何已过构造校验，提交前不重新推导 header/array 的几何（崩溃安全四结构序列见
    /// `table::commit_table`）。add_entry_at 的增量写不经本几何，走 `table::commit_gpt`。
    /// 备份头位置（容器末 LBA）取自构造时传入的 `file_last_lba`——同一事实不接受第二个来源，
    /// 调用点不得各自重算一遍 `src.size / ss - 1`
    pub fn commit(&self, src: &mut crate::dev::FileSource) -> io::Result<()> {
        crate::table::commit_table(src, self.ss, &self.header, &self.entries, self.file_last_lba)
    }
}

/// 已定义条目之间的重叠探测：按起点排序后相邻比较即可覆盖全部重叠对
/// （A 与 C 重叠而 B 夹在中间时，B 必与 A 或 C 之一重叠）。
///
/// 返回第一个重叠对的两个 **1-based 分区号**（= 数组槽位号），与 CLI 的 `:N` 一致。
/// 未使用条目的判据与解析层一致：两个 LBA 字段同时为零
pub fn find_overlap(entries: &[GPTPartitionEntry]) -> Option<(usize, usize)> {
    let mut defined: Vec<(u64, u64, usize)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| !(e.starting_lba == 0 && e.ending_lba == 0))
        .map(|(i, e)| (e.starting_lba, e.ending_lba, i + 1))
        .collect();
    defined.sort_by_key(|&(start, end, num)| (start, end, num));
    for w in defined.windows(2) {
        let (_, prev_end, prev_num) = w[0];
        let (next_start, _, next_num) = w[1];
        if next_start <= prev_end {
            return Some((prev_num, next_num));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(start: u64, end: u64) -> GPTPartitionEntry {
        GPTPartitionEntry {
            partition_type_guid: [1; 16],
            unique_partition_guid: [2; 16],
            starting_lba: start,
            ending_lba: end,
            attribute_bits: 0,
            partition_name: "".into(),
        }
    }

    // 条目数组几何的构造判据与跨度/槽位换算已随类型下沉 codec 层（`table::tests`），
    // 此处只保留本模块自己的不变量

    /// 重叠探测：相邻、嵌套、跨越三形态都要命中；未使用条目与紧邻不重叠不得误报
    #[test]
    fn overlap_detection() {
        assert_eq!(find_overlap(&[]), None);
        // 空槽位（两个 LBA 同时为零）不参与
        assert_eq!(find_overlap(&[entry(0, 0), entry(100, 200)]), None);
        // 紧邻（200 与 201）不重叠
        assert_eq!(find_overlap(&[entry(100, 200), entry(201, 300)]), None);
        // 相邻接触（200 与 200）即重叠
        assert_eq!(find_overlap(&[entry(100, 200), entry(200, 300)]), Some((1, 2)));
        // 嵌套：150..250 落在 100..200 内
        assert_eq!(find_overlap(&[entry(100, 200), entry(150, 250)]), Some((1, 2)));
        // 乱序输入：探测前排序，报告的是槽位号而非输入次序
        assert_eq!(find_overlap(&[entry(150, 250), entry(1, 50), entry(100, 200)]), Some((3, 1)));
        // 跨过中间者：A 与 C 重叠、B 在 A 内 → 报出先出现的那一对
        assert_eq!(find_overlap(&[entry(100, 400), entry(120, 130), entry(300, 500)]), Some((1, 2)));
    }
}