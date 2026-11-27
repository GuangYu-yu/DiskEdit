# 01-specs/specs-uefi（23 行）

## 判断
- 认领 22：GPT 布局/解析/写入各条均由 `crates/core/src/table/gpt/*`、`create.rs` 与 case_runner/equivalence 测试锚定。
- 差异 1：GPT-011（PMBR Disk Signature 清零）。

## 差异 GPT-011
- 行为：重写 PMBR 保留 0..446（引导代码+磁盘签名），不按 UEFI §5.2.3 把 Unique MBR Disk Signature 清零；新建空盘时该区域自然为零。
- 依据：`crates/core/src/table/create.rs::protective_mbr_image`（注释"保留 0..446"）；基线二进制等价测试（G7）证实与参考行为一致。
- D-建议：保留 0..446 属有意的兼容策略（Windows/fdisk 期望签名稳定），不建议改。
- 撤回条件：出现强制要求 PMBR 磁盘签名为零的外部契约（如某些嵌入式引导链校验），或基线行为改为清零时，改写 `protective_mbr_image` 并补测试。
