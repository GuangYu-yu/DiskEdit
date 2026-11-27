# sites-e2fsprogs-check-refusal

## 判断
e2fsck 内部拒绝、problem 表字符串与 fix_problem 站点（272 行）。全批 271 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001/002；B4/B17 占用闸；SA1-002 只读

## 撤回条件
同 message-entry 批；另：若占用闸放宽到允许写占用中目标，check_mount 场景需重审。

## 换版重放补充（2026-11-27：e2fsprogs 1.47.0→1.47.4）

- 本批新增/改写 3 行（00767/00768/01168，ehandler Ignore/Force rewrite 交互应答、PR_2_DIR_CORRUPTED），全部判**裁剪**：e2fsck 修复面与交互面，只消费退出码（SA2-001/002）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：同 message-entry 批；另：若占用闸放宽到允许写占用中目标，check_mount 场景需重审。
