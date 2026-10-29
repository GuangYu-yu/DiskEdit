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
///
/// 字段全部私有：条目只能经 [`ValidatedGeometry::update_entry`] 修改，该入口在写回前
/// 按构造期同一判据复核。这样"构造后改坏几何"在类型外写不出来
#[derive(Clone)]
pub struct ValidatedGeometry {
    /// 该表自身的扇区大小（条目 LBA 的单位）
    ss: u64,
    /// 修复后将生效的头（last_usable_lba / backup_lba 已收敛）
    header: RawHeader,
    /// 分区条目，长度恒等于 `entry_geometry.entry_count`
    entries: Vec<GPTPartitionEntry>,
    /// 构造时读到的条目，未经本层的编辑：提交前与盘上现状比对的基准
    decided_entries: Vec<GPTPartitionEntry>,
    /// 构造时读到的头，未经修复动作收敛：提交前与盘上现状比对的基准
    decided_header: RawHeader,
    /// 经 [`Self::update_entry`] 改过的槽位下标：比对时这些槽位允许已是目标样子
    dirty: Vec<usize>,
    entry_geometry: EntryArrayGeometry,
    /// 容器最后一个 LBA（本表 ss 口径）：几何自洽性的上界，也是 checkpoint 的 LBA 上界
    file_last_lba: u64,
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
        // 条目数组的长度必须与头自述的条目数一致：不一致时下游的槽位→下标换算与
        // 全表重写会一边按 entry_count 补零/截断，一边按 entries.len() 取号
        if g.entries.len() != entry_geometry.entry_count as usize {
            return Err(invalid(format!(
                "GPT entry array holds {} entries but the header declares {}",
                g.entries.len(),
                entry_geometry.entry_count
            )));
        }
        // 修复后的 last_usable 必须覆盖 first_usable 与所有已定义条目的末端，且不得
        // 侵入盘尾备份元数据（与 table 解析层的 validate_geometry 同一上界公式）。
        // 构造点把这些判据收进来，消费者不必再各自复核
        let max_end = g
            .entries
            .iter()
            .filter(|e| e.ending_lba != 0)
            .map(|e| e.ending_lba)
            .max()
            .unwrap_or(0);
        let last_usable = post_repair_last_usable.unwrap_or(g.header.last_usable_lba);
        if last_usable < g.header.first_usable_lba || last_usable < max_end {
            return Err(invalid(format!(
                "last_usable_lba {last_usable} does not cover usable start {} / highest partition end {max_end}",
                g.header.first_usable_lba
            )));
        }
        let ceiling = file_last_lba
            .checked_sub(entry_geometry.lba_span())
            .and_then(|v| v.checked_sub(1))
            .ok_or_else(|| invalid("container too small for the backup GPT metadata"))?;
        if last_usable > ceiling {
            return Err(invalid(format!(
                "last_usable_lba {last_usable} would overlap the backup GPT metadata (ceiling {ceiling})"
            )));
        }
        let mut header = g.header.clone();
        if let Some(last_usable) = post_repair_last_usable {
            header.last_usable_lba = last_usable;
        }
        // 修复后备份头位于容器末端；无修复动作时盘上值已等于它，赋值是恒等变换
        header.backup_lba = file_last_lba;
        Ok(Self {
            ss: g.ss,
            header,
            entries: g.entries.clone(),
            decided_entries: g.entries.clone(),
            decided_header: g.header.clone(),
            dirty: Vec::new(),
            entry_geometry,
            file_last_lba,
        })
    }

    pub fn first_usable_lba(&self) -> u64 {
        self.header.first_usable_lba
    }

    /// 修复后将生效的可用区上界。唯一来源：调用点只从这里取，不自行推导
    pub fn last_usable_lba(&self) -> u64 {
        self.header.last_usable_lba
    }

    /// 该表自身的扇区大小：条目 LBA 的换算单位（可与容器 ss 不同）
    pub fn sector_size(&self) -> u64 {
        self.ss
    }

    /// 盘 GUID：`copy` 一类命令用它识别同源表
    pub fn disk_guid(&self) -> [u8; 16] {
        self.header.disk_guid
    }

    /// 只读的条目视图。修改必须走 [`Self::update_entry`]
    pub fn entries(&self) -> &[GPTPartitionEntry] {
        &self.entries
    }

    /// 修改一条条目的唯一入口。闭包拿到副本，写回前按构造期同一判据复核：
    /// 端点不倒挂、落在 usable 区内、与其余已定义条目不重叠。任一违反即保留旧条目并报错
    pub fn update_entry(
        &mut self,
        index: usize,
        f: impl FnOnce(&mut GPTPartitionEntry),
    ) -> io::Result<()> {
        let Some(cur) = self.entries.get(index) else {
            return Err(invalid(format!("partition entry #{index} is out of range")));
        };
        let mut draft = cur.clone();
        f(&mut draft);
        // 未使用条目的判据与解析层一致：两个 LBA 字段同时为零
        if !(draft.starting_lba == 0 && draft.ending_lba == 0) {
            if draft.starting_lba > draft.ending_lba {
                return Err(invalid(format!(
                    "entry #{} has an inverted range {}..{}",
                    index + 1,
                    draft.starting_lba,
                    draft.ending_lba
                )));
            }
            if draft.starting_lba < self.header.first_usable_lba
                || draft.ending_lba > self.header.last_usable_lba
            {
                return Err(invalid(format!(
                    "entry #{} range {}..{} falls outside the usable area {}..{}",
                    index + 1,
                    draft.starting_lba,
                    draft.ending_lba,
                    self.header.first_usable_lba,
                    self.header.last_usable_lba
                )));
            }
            for (i, other) in self.entries.iter().enumerate() {
                if i == index || (other.starting_lba == 0 && other.ending_lba == 0) {
                    continue;
                }
                if draft.starting_lba <= other.ending_lba && other.starting_lba <= draft.ending_lba {
                    return Err(invalid(format!(
                        "entry #{} would overlap entry #{}",
                        index + 1,
                        i + 1
                    )));
                }
            }
        }
        self.entries[index] = draft;
        if !self.dirty.contains(&index) {
            self.dirty.push(index);
        }
        Ok(())
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

    /// 盘上的条目与头仍与本几何的依据一致：每个槽位要么是构造时读到的样子，要么（对本层
    /// 改过的槽位）已是 [`Self::entries`] 里的目标样子；头的每个字段同理——要么停在
    /// 构造时读到的那份 `decided_header`，要么已收敛到本层的 `header`（修复动作会改写
    /// 位置与 usable 区间，那些正是本层的目标值）。两者都不是的那一处只能来自窗口内的
    /// 第三方改动——提交写的是快照，不核对就会把它的改动静默覆盖（条目消失、搬移落点
    /// 按旧几何算、第三方改的 disk GUID / usable 区间被写回旧值）
    ///
    /// 窗口的长度由调用点决定：add/rename/set/del 是"读 → 判 → 修复 → 提交"，
    /// resize 与搬移类命令则跨整个 FS 调整或数据搬移
    pub(crate) fn ensure_entries_unchanged(&self, src: &crate::dev::FileSource) -> io::Result<()> {
        let g = crate::table::load_gpt(src)
            .map_err(crate::table::into_io_error)?
            .ok_or_else(|| invalid("GPT vanished before the table write"))?;
        if g.entries.len() != self.decided_entries.len() {
            return Err(invalid(format!(
                "GPT entry array holds {} entries but {} were validated — the table changed outside this tool",
                g.entries.len(),
                self.decided_entries.len()
            )));
        }
        // 位置字段（MyLBA / PartitionEntryLBA）由提交规范化，不作为"内容被改"的判据；
        // 其余头字段一旦既不是原值也不是目标值，就是第三方在这个窗口里改过表头
        let (d, now, target) = (&self.decided_header, &g.header, &self.header);
        let hex = |g: &[u8; 16]| format!("{g:02x?}");
        let changed = [
            ("revision", d.revision.to_string(), now.revision.to_string(), target.revision.to_string()),
            ("backup_lba", d.backup_lba.to_string(), now.backup_lba.to_string(), target.backup_lba.to_string()),
            (
                "first_usable_lba",
                d.first_usable_lba.to_string(),
                now.first_usable_lba.to_string(),
                target.first_usable_lba.to_string(),
            ),
            (
                "last_usable_lba",
                d.last_usable_lba.to_string(),
                now.last_usable_lba.to_string(),
                target.last_usable_lba.to_string(),
            ),
            ("disk_guid", hex(&d.disk_guid), hex(&now.disk_guid), hex(&target.disk_guid)),
            (
                "number_of_partition_entries",
                d.number_of_partition_entries.to_string(),
                now.number_of_partition_entries.to_string(),
                target.number_of_partition_entries.to_string(),
            ),
            (
                "size_of_partition_entry",
                d.size_of_partition_entry.to_string(),
                now.size_of_partition_entry.to_string(),
                target.size_of_partition_entry.to_string(),
            ),
            ("header_size", d.header_size.to_string(), now.header_size.to_string(), target.header_size.to_string()),
        ]
        .into_iter()
        .find(|(_, was, is, want)| is != was && is != want);
        if let Some((field, was, is, _)) = changed {
            return Err(invalid(format!(
                "GPT header field {field} changed outside this tool since the table was validated \
                 ({was} → {is}) — refusing to overwrite it; re-run the command"
            )));
        }
        for (i, entry) in g.entries.iter().enumerate() {
            if *entry == self.decided_entries[i] {
                continue;
            }
            if self.dirty.contains(&i) && *entry == self.entries[i] {
                continue;
            }
            return Err(invalid(format!(
                "partition entry #{} changed outside this tool since the table was validated — \
                 refusing to overwrite it; re-run the command",
                i + 1
            )));
        }
        Ok(())
    }

    /// 用本几何提交分区表。几何已过构造校验，提交前不重新推导 header/array 的几何
    /// （崩溃安全四结构序列见 `table::commit_table`）。备份头位置（容器末 LBA）取自构造时
    /// 传入的 `file_last_lba`——同一事实不接受第二个来源，调用点不得各自重算一遍
    /// `src.size / ss - 1`
    pub fn commit(&self, src: &mut crate::dev::WriteSource) -> io::Result<()> {
        // 全表写入之前先确认盘上的表仍是本几何的依据（见
        // [`Self::ensure_entries_unchanged`]）
        self.ensure_entries_unchanged(src)?;
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
