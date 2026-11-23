//! 持久记录：16 字节头、帧编解码与纯前缀恢复。
//!
//! 记录只追加，帧序号连续。载荷固定前缀描述目标身份与计划摘要，随后是帧。
//! 恢复器不读目标、不看时钟：输入为记录字节与当前指纹，输出为确定的恢复结论。

pub mod codec;
pub mod frame;
pub mod header;
pub mod recover;

pub use codec::{DecodedRecord, RecordError, decode, encode};
pub use frame::{Frame, FrameKind, Preimage};
pub use header::{HEADER_LEN, MAGIC, RecordHeader};
pub use recover::{RecoveryPlan, recover};

use crate::fingerprint::Fingerprint;

/// 记录载荷固定前缀的字节数：指纹、几何代次、计划摘要、前缀 CRC。
pub const BODY_LEN: usize = 32 + 8 + 32 + 4;

/// 载荷声明长度上限，字节。
pub const MAX_PAYLOAD_BYTES: u32 = 16 * 1024 * 1024;

/// 载荷固定前缀：这份记录属于谁、对应哪一版几何与哪一份计划。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordBody {
    pub target: Fingerprint,
    pub geometry_generation: u64,
    pub plan_digest: [u8; 32],
}
