# 03-docs/docs-f2fs-tools（1 行）

## 判断
- 裁剪 1（00001）：sload.f2fs 压缩装载不在依赖面；f2fs 面仅 mkfs/fsck/resize 登记（SA2-010/012/013），f2fs 收缩按不支持拒绝（SA2-010）。

## 撤回条件
- 若 sload.f2fs 或 f2fs 收缩纳入档案表，重开。
