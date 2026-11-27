# 处置说明：libfdisk confirm（sfdisk 交互与 --force）

- 批次：`04-implicit/imp-libfdisk-confirm`（参考账本 implicit）

## 判断

交互提示/wipemode/脚本 force_label 裁剪；--force 跳过全部一致性检查拒绝（安全边界不设总旁路）。

## 依据

SA1-005/SA1-010；src/app/cli/parse.rs::--yes。

## 撤回条件

撤回条件：出现必须跳过全部校验的外部契约（如灾后抢写）时，重新评估受控旁路（须带写后回读与记录）。
