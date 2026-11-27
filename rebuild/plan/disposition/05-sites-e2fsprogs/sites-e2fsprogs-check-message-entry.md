# sites-e2fsprogs-check-message-entry

## 判断
e2fsck problem 表 311 行：消息、PROMPT_* 交互应答、fix_problem 判读。全批 311 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001/002（不做 fs 修复，只消费退出码）；交互面裁剪（e2fsck -f -n / -fp 非交互，src/platform/linux/tools.rs::check_arguments）；SA2-001 定次序

## 撤回条件
本实现引入任何 fs 修复能力，或出现必须复刻 problem 表文案/交互应答的外部契约（须先按 §10 登记）时重审。

## 换版重放补充（2026-11-27：e2fsprogs 1.47.0→1.47.4）

- 本批新增/改写 5 行（01547/01548/01647/01689/01720，problem 表新条目：ea_inode 族与 orphan_present clean），全部判**裁剪**：消息与 PROMPT_* 交互应答属 e2fsck 修复面，本实现只消费退出码（SA2-001/002），check 形态 e2fsck -f -n 非交互。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：本实现引入任何 fs 修复能力，或出现必须复刻 problem 表文案/交互应答的外部契约（须先按 §10 登记）时重审。
