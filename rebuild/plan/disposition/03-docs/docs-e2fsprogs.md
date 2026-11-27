# 03-docs/docs-e2fsprogs（17 行）

## 判断
- 认领 1（00003）：dumpe2fs -h 在回读面（SA2-003 尺寸回读须填满目标；SA2-011 只解析文档化稳定字段）。
- 裁剪 16：resize2fs/tune2fs BUGS 自述、badblocks 破坏性写模式（与 SA1-006 取向一致）、e4defrag、mklost+found——均不在依赖面或无行为面。

## 依据
- 参考行原文（man BUGS 节）多为上游工具内部限制；tune2fs/dumpe2fs 在档案表的登记见 `crates/core/src/fs/catalog.rs`。

## 撤回条件
- 若 badblocks/e4defrag/mklost+found 纳入依赖面，重开对应行。
