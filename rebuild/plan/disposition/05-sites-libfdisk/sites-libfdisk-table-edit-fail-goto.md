# sites libfdisk table edit fail goto 处置说明

判断：批内裁剪行绑定的参考行为是 fdisk/cfdisk/sfdisk 的交互面（菜单、ask 回调、prompt、TUI、交互脚本输入）。本实现为非交互单命令 CLI，不提供这些特性。
依据：特性依据=交互面；SA1-010（输入面定案）、SA1-022（防御面口径）。
撤回条件：决定引入交互式界面（TUI/向导）时按 ask 回调面重建；或出现必须依赖交互提示的外部契约（无人值守以外场景）时重评。
补充：bsd/sgi 行按 SA1-016 裁剪；backup_sectors 行按 SA1-020 裁剪；table_to_string 差异见列式表格登记。

## 换版重放补充（2026-11-27：libfdisk/util-linux 2.39.3→2.42.4）

- 本批新增/改写 19 行：认领 4 行（00575–00578，2.42.4 新增 fdisk_partition_get_max_size API——resize 上界计算已有：src/app/decide/resize.rs grow ceiling、crates/core/src/table/edit.rs gaps/first_fit，SA1-008/014）；裁剪 15 行（00903 与 00899–00901 sfdisk --discard-free 为 2.42.4 新增 discard 面，SA1-006；00977–00985 fdisk 菜单 resize 对话框；00992–01002 fdisk 菜单 discard 对话框——交互面）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：决定引入交互式界面（TUI/向导）时按 ask 回调面重建；或出现必须依赖交互提示的外部契约（无人值守以外场景）时重评。重开 discard 面（须带二次确认与不可恢复提示）或引入交互界面时逐行重审。
