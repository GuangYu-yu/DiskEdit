# sites libfdisk table edit assert disk utils 处置说明

判断：批内裁剪行绑定的参考行为是 fdisk/cfdisk/sfdisk 的交互面（菜单、ask 回调、prompt、TUI、交互脚本输入）。本实现为非交互单命令 CLI，不提供这些特性。
依据：特性依据=交互面；SA1-010（输入面定案）、SA1-022（防御面口径）。
撤回条件：决定引入交互式界面（TUI/向导）时按 ask 回调面重建；或出现必须依赖交互提示的外部契约（无人值守以外场景）时重评。
补充：sfdisk --backup 行按 SA1-020 裁剪；sfdisk_print_partition 差异见列式表格登记。

## 换版重放补充（2026-11-27：libfdisk/util-linux 2.39.3→2.42.4）

- 本批新增/改写 1 行（00976 fdisk 菜单 resize 对话框 assert，2.42.4 新增交互面），判**裁剪**：交互面未实现；resize 能力本身已实现（SA1-014）。其余新增行（00854 delete 解析面、00956 --sector-size）判认领，见同名 tsv。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：决定引入交互式界面（TUI/向导）时按 ask 回调面重建；或出现必须依赖交互提示的外部契约（无人值守以外场景）时重评。
