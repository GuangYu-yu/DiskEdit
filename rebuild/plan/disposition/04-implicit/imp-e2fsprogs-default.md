# 处置说明：e2fsprogs default（常数群）

- 批次：`04-implicit/imp-e2fsprogs-default`（参考账本 implicit）

## 判断

整批裁剪：编译宏/内部布局/调度常数/discard 步进均不复制；discard 另有 SA1-006 范围裁剪。

## 依据

SA1-006/SA2-001/002/003；crates/core/src/fs/catalog.rs。

## 撤回条件

撤回条件：若自研 ext 布局知识（探针级），先补 SA2-003 回读口径的用例。

## 换版重放补充（2026-11-27，e2fsprogs 1.47.0→1.47.4）

### 判断
- 裁剪 2（00320/00321）：EXT4_MAX/DEFAULT_ORPHAN_FILE_SIZE（8MiB/2MiB）ext2fs 内部布局常数；尺寸边界由工具+回读承担（SA2-001/002/003），同 00313 系口径。

### 依据
- lib/ext2fs/ext2fs.h:1822-1823 属 orphan file 特性内部常数，不在调用面。

### 撤回条件
- 若自研 ext 布局知识（探针级），先补 SA2-003 回读口径的用例。
