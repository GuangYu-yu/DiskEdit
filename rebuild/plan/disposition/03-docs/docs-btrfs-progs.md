# 03-docs/docs-btrfs-progs（138 行）

## 判断
- 认领 15：label（00018-00020，btrfs attrs 面 catalog.rs）、resize 语义（00028-00036，SA2-006/007/008/016）、min-dev-size 等价的失败拒绝（00062，SA2-007/016）、mkfs label（00094）、mkfs 拒绝覆盖取向（00104，SA1-005）。
- 裁剪 123：defrag/du/mkswapfile/show/sync/usage/check --repair/man5 balance/mkfs 上游选项面/btrfstune——均不在依赖面或属上游选项/显示面。

## 依据
- 档案表登记见 `crates/core/src/fs/catalog.rs`；resize 契约见 SA2-006/007/008/016 与 tests/online.rs。

## 建议拆分
- resize 认领行（00028-00036/00062）与大量上游选项裁剪行混批：可拆 `docs-btrfs-resize`（认领面）与 `docs-btrfs-misc`（裁剪面），已在 00028/00080 行 note 标注建议，不强拆。

## 撤回条件
- 若 defrag/usage/mkswapfile/btrfstune 纳入档案表，重开对应行。
