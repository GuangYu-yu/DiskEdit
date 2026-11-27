# 处置说明：parted default（#define 常数群）

- 批次：`04-implicit/imp-parted-default`（参考账本 implicit）

## 判断

GPT/MBR 几何与签名常数、CHS 上限认领（SA1-019/SA1-027/SA1-028）；老盘标（amiga/aix/pc98/rdb）裁剪（SA1-016）；内嵌文件系统布局常数（ext2/f2fs/fat/hfs/reiserfs/xfs）、废弃 ioctl、UI 词法常数裁剪。

## 依据

SA1-016/SA1-017/SA2 系；crates/core/src/table/{create,edit,mbr,gpt}。

## 撤回条件

撤回条件：若引入内嵌文件系统尺寸探测或老盘标识别，须先补档案表只读探针与拒绝矩阵（SA1-017 撤回条件同款）。
