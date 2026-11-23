//! MBR：主分区表、保护性 MBR 与引导签名。

pub mod extended;
pub mod primary;

pub use extended::walk_extended;
pub use primary::{Mbr, MbrEntry, protective_mbr, size_in_lba};
