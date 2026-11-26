//! 文件系统档案表：识别种类、内容类别、能力矩阵与程序序列。

pub mod catalog;
pub mod profile;
pub mod program;

pub use catalog::{
    MountNeed, OnlineGrow, Profile, ShrinkScope, Support, Verb, online_grow, profile,
};
pub use profile::{ContentKind, FsKind, MkfsKind};
pub use program::{Access, Determinism, Program};
