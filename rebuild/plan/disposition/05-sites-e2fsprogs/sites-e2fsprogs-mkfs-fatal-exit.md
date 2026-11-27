# sites-e2fsprogs-mkfs-fatal-exit

## 判断
mke2fs usage/OOM 出口（128 行）。全批 116 行已逐行翻态，note 见同名 tsv。

## 依据
mkfs_invocation 固定参数（-t extN [-L label]），usage 不可达

## 撤回条件
扩大参数面或依赖 usage 码的外部契约出现时重审。

## 换版重放补充（2026-11-27：e2fsprogs 1.47.0→1.47.4）

- 本批新增/改写 17 行（00361/00363/00371/00373/00375/00427/00429/00435/00467/00511/00515/00588/00590/00592/00609/00666/00685），全部判**裁剪**：mke2fs usage/OOM/内部出口随外部工具自带；对应参数面（revision/特性组合/xattr selinux/块大小/huge files）不接线，usage 出口不可达（mkfs_invocation 固定参数）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：扩大参数面或出现依赖 usage 退出码的外部契约时重审。
