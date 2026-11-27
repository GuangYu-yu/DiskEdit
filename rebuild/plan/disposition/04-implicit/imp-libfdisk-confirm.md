# 处置说明：libfdisk confirm（sfdisk 交互与 --force）

- 批次：`04-implicit/imp-libfdisk-confirm`（参考账本 implicit）

## 判断

交互提示/wipemode/脚本 force_label 裁剪；--force 跳过全部一致性检查拒绝（安全边界不设总旁路）。

## 依据

SA1-005/SA1-010；src/app/cli/parse.rs::--yes。

## 撤回条件

撤回条件：出现必须跳过全部校验的外部契约（如灾后抢写）时，重新评估受控旁路（须带写后回读与记录）。

## 换版重放补充（2026-11-27，util-linux 2.39.3→2.42.4）

### 判断
- 裁剪 1（libfdisk-imp-00041）：sfdisk 脚本 last-lba 头+force 改写分支；脚本/JSON 导入不提供（SA1-010），同 00022 force_label 先例。

### 依据
- disk-utils/sfdisk.c:2016 属 script 导入路径；SA1-010 范围裁剪。

### 撤回条件
- 若引入脚本/JSON 导入面（SA1-010 翻案），重开本行并补身份校验与冲突处理。
