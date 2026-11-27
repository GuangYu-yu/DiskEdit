//! GPT：头部、条目、解析与序列化。

pub mod entries;
pub mod header;
pub mod parse;
pub mod write;

pub use entries::{PartitionEntry, PartitionName};
pub use header::GptHeader;
pub use parse::{GptCopy, GptEvidence, GptState, HeaderIssue};
