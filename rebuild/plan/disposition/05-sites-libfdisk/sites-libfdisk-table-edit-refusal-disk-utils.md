# 05-sites-libfdisk/sites-libfdisk-table-edit-refusal-disk-utils（换版重放批次，91 行）

判断沿承行 note 与同名旧批判断（sources.tsv 2026-11-27 换版重放）；逐行 state 已迁移，本批为归类重建。

## 撤回条件

沿承行 note 引用的 SA/D 条目撤回条件；本批仅归类调整，不改变判断。

## 换版重放补充（2026-11-27：libfdisk/util-linux 2.39.3→2.42.4）

- 本批新增/改写 14 行：认领 4 行（00853 delete 解析拒绝 SA1-015/027；00955 与 01128 --sector-size 校验 R0092；01119 cfdisk 撞签提示——检测语义已实现 SA1-005/006）；裁剪 10 行（00902/00904/00905 --discard-free 新 discard 面 SA1-006；00980 fdisk 菜单 resize 对话框——上界拒绝语义已有 SA1-014；00991/00997/01003/01004 fdisk 菜单 discard 对话框；01118/01126 cfdisk TUI 提示/帮助面）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：决定引入交互式界面（TUI/向导）时按 ask 回调面重建；或出现必须依赖交互提示的外部契约（无人值守以外场景）时重评。重开 discard 面时重审对应行。
