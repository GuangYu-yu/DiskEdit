# sites libfdisk table edit error report 处置说明

判断：批内裁剪行绑定的参考行为是 fdisk/cfdisk/sfdisk 的交互面（菜单、ask 回调、prompt、TUI、交互脚本输入）。本实现为非交互单命令 CLI，不提供这些特性。
依据：特性依据=交互面；SA1-010（输入面定案）、SA1-022（防御面口径）。
撤回条件：决定引入交互式界面（TUI/向导）时按 ask 回调面重建；或出现必须依赖交互提示的外部契约（无人值守以外场景）时重评。
补充：sun/sgi/bsd 行按 SA1-016 裁剪；print_partition_info 差异见列式表格登记。

## 换版重放补充（2026-11-27：libfdisk/util-linux 2.39.3→2.42.4）

- 本批新增/改写 7 行：认领 3 行（00920/01013 为 2.42.4 新增 append 撞签提示——撞签检测与拒绝已实现，SA1-005/006；00948 --show-pt-geometry 废弃别名——info JSON 几何面 + 未知选项 UsageError，SA1-009/010）；裁剪 4 行（00984/00993/00995/01001 绑定 2.42.4 新增 fdisk 菜单 resize/discard 对话框——交互面 + discard 面 SA1-006）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：决定引入交互式界面（TUI/向导）时按 ask 回调面重建；或出现必须依赖交互提示的外部契约（无人值守以外场景）时重评。引入签名擦除选项时重审撞签认领行；重开 discard 面（须带二次确认与不可恢复提示）时重审 discard 行。
