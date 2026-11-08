//! 编译期不变量：本文件**刻意不包含任何测试**。
//!
//! 原因（不是环境限制，是当前工程结构决定的）：
//!
//! 1. 本包只有一个二进制目标 `DiskEdit`（`Cargo.toml` 没有 `[lib]`）。集成测试
//!    （`tests/*.rs`）只能链接库目标或启动二进制，无法 `use` 到 `src/` 里的内部类型，
//!    因此"`CleanupPermit` 只能由 `RecoveryEngine` 铸造""`RelativePath` 不能从任意
//!    字符串构造"这类类型层面的不变量，在集成测试里既无法写正例也无法写反例。
//!
//! 2. 主线已明确**不批准**新增 `trybuild`、**不批准**新增 `[lib]` 目标：改变 crate
//!    结构的代价大于收益。因此本文件维持空测试集，不写编译应失败的用例。
//!
//! 3. 编译期不变量要真正落地，需要二者之一：给包加 `[lib]` 目标并让内部类型对集成
//!    测试可见，或把这些不变量下沉为 `src/` 内的单元测试（例如
//!    `src/recovery/cleanup.rs` 的 `#[cfg(test)]`）。两者都超出"只写 `tests/`"的范围。
//!
//! 如果将来重新批准加库目标 + `trybuild`，本文件应承载的用例：
//!
//! - `CleanupPermit` 无法在 `src/recovery/cleanup.rs` 之外构造：
//!   删除只经引擎签发许可后调用。
//! - `RelativePath` 只有经过父链校验的构造入口：父链逐级
//!   `O_NOFOLLOW|O_DIRECTORY`，任一级符号链接/非目录 → `NotOurs`）。
//! - 未实现 `ChunkRequest` 的请求类型不能调用 `chunk_logger`（当前 `src/cmd/layout.rs`
//!   的 `ResizePartRequest` 编译错误正说明这条约束生效）。
