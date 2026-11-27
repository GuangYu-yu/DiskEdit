# sites libfdisk table edit fatal exit 处置说明

判断：批内裁剪行绑定的参考行为是 fdisk/cfdisk/sfdisk 的交互面（菜单、ask 回调、prompt、TUI、交互脚本输入）。本实现为非交互单命令 CLI，不提供这些特性。
依据：特性依据=交互面；SA1-010（输入面定案）、SA1-022（防御面口径）。
撤回条件：决定引入交互式界面（TUI/向导）时按 ask 回调面重建；或出现必须依赖交互提示的外部契约（无人值守以外场景）时重评。

## 换版重放补充（2026-11-27：libfdisk/util-linux 2.39.3→2.42.4）

- 本批新增/改写 14 行：认领 5 行（00854 delete 分区号校验 SA1-015/027；00858/00859 dump 面 info JSON SA1-010；00956/01129 --sector-size 校验 R0092）；拒绝 2 行（00856/00857 sfdisk --reorder——条目重排不提供，SA3-001 同原则：身份与次序不静默改写）；裁剪 7 行（00863/00864 --backup-pt-sectors 备份文件面 SA1-020；00899–00901 --discard-free 新 discard 面 SA1-006；00925 prompt 面；01131 cfdisk TUI）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：决定引入交互式界面（TUI/向导）时按 ask 回调面重建；或出现必须依赖交互提示的外部契约（无人值守以外场景）时重评。重开 discard 面或备份文件面（SA1-020）时重审对应行；出现依赖槽位重排次序的外部契约时重评 --reorder 拒绝行。
