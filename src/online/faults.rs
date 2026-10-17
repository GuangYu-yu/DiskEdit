//! 在线路径的崩溃注入点（sfdisk 前后各一）。


// 在线路径的成对注入点：表是否已落盘全看这一对之间的那次 sfdisk
crate::movepart::fault_points! {
    /// sfdisk 之前：此刻 abort，分区表确定未改
    fault_online_before_write() = "online-before-write";
    /// sfdisk 之后：此刻 abort，分区表可能已改
    fault_online_after_write() = "online-after-write";
}