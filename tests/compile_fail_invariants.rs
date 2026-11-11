//! 本文件不含测试：包只有二进制目标 `DiskEdit`（`Cargo.toml` 没有 `[lib]`），
//! 集成测试（`tests/*.rs`）无法 `use` 到 `src/` 的内部类型，类型层面的不变量
//! （`CleanupPermit` 只能由 `RecoveryEngine` 铸造、`RelativePath` 不能从任意字符串
//! 构造）在这里既写不出正例也写不出反例。
//!
//! 这类不变量要落地，需要给包加 `[lib]` 目标让内部类型对集成测试可见，或把它们
//! 下沉为 `src/` 内的单元测试（例如 `src/recovery/cleanup.rs` 的 `#[cfg(test)]`）。
//!
//! 待覆盖的用例：
//!
//! - `CleanupPermit` 无法在 `src/recovery/cleanup.rs` 之外构造：删除只经引擎签发许可后调用。
//! - `RelativePath` 只有经过父链校验的构造入口：父链逐级
//!   `O_NOFOLLOW|O_DIRECTORY`，任一级符号链接/非目录 → `NotOurs`。
//! - 未实现 `ChunkRequest` 的请求类型不能调用 `chunk_logger`
//!   （`src/cmd/layout.rs` 的 `ResizePartRequest` 会因此编译失败）。
