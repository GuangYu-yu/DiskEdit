# 处置说明：ntfs-3g confirm（-f/Recover）

- 批次：`04-implicit/imp-ntfs-3g-confirm`（参考账本 implicit）

## 判断

ntfsresize/ntfsfix/ntfslabel 的 -f 语义认领（SA2-009/SA2-012，档案表固定参数）；ntfsinfo/ntfscluster 调试面裁剪。

## 依据

SA2-009/SA2-012；crates/core/src/fs/program.rs。

## 撤回条件

撤回条件：若引入 ntfs 调试探针，先补档案表只读登记。
