# 处置说明：gdisk misc error-report（属性/fixparts/类型库）

- 批次：`05-sites-gdisk/sites-gdisk-misc-error-report`（参考账本 sites/gdisk）

## 判断

属性位解析错误拒绝认领（edit.rs::attribute_bit）；UUID 降级警告认领为本实现更保守（SA3-001）；fixparts/类型库/BSD 行裁剪；ReverseBytes 分配拒绝。

## 依据

SA1-001/SA3-001/SA1-016。

## 撤回条件

撤回条件：若纳入 fixparts（MBR 空隙整理）或动态类型库，另立命令面工作项。
