# 03-docs/docs-btrfs-progs（181 行）

## 判断
- 认领 15：label（00018-00020，btrfs attrs 面 catalog.rs）、resize 语义（00028-00036，SA2-006/007/008/016）、min-dev-size 等价的失败拒绝（00062，SA2-007/016）、mkfs label（00094）、mkfs 拒绝覆盖取向（00104，SA1-005）。
- 裁剪 123：defrag/du/mkswapfile/show/sync/usage/check --repair/man5 balance/mkfs 上游选项面/btrfstune——均不在依赖面或属上游选项/显示面。

## 依据
- 档案表登记见 `crates/core/src/fs/catalog.rs`；resize 契约见 SA2-006/007/008/016 与 tests/online.rs。

## 建议拆分
- resize 认领行（00028-00036/00062）与大量上游选项裁剪行混批：可拆 `docs-btrfs-resize`（认领面）与 `docs-btrfs-misc`（裁剪面），已在 00028/00080 行 note 标注建议，不强拆。

## 撤回条件
- 若 defrag/usage/mkswapfile/btrfstune 纳入档案表，重开对应行。

## 换版重放补充（2026-11-27，btrfs-progs 6.6.3→7.3）

### 判断
- 认领 3（00034/00035/00037）：resize 次序契约（先扩分区再扩 fs、先缩 fs 再缩分区=本实现 grow 表→内核视图→fs / shrink fs 成功才落表，SA2-006/007/016+tests/online.rs）；扩容完成以 superblock total_bytes 回读证实（SA2-006）；低于最小值由工具失败拒绝继续、分区表不动（SA2-007，min-dev-size 探测面不提供）。
- 裁剪 54：defragment（00006/00007/00011）、mkswapfile/swapfile 族（00022/00024/00025/00027）、文件镜像 --offline resize（00031/00041/00042）、--enqueue（00040）、commit-stats 显示面（00048/00049/00051）、filesystem sync 子命令（00052，落盘由写路径 sync_all 承担 SA1-011）、排版行（00066）、check 面注记（00073/00079）、mkfs.btrfs 选项/特性位/示例行（00105-00107/00111/00113-00134/00146/00150-00155）、btrfstune（00170/00175/00181）。

### 依据
- resize 三行与既有认领行 00030/00036/00038/00039/00072 同口径；btrfs 事实源=superblock 回读（SA2-006）。

### 撤回条件
- 若 defrag/mkswapfile/btrfstune/usage 显示或文件镜像载体纳入档案表，重开对应行。
- 若 min-dev-size 探测纳入实现面，重开 00037 并补探测拒绝矩阵。
