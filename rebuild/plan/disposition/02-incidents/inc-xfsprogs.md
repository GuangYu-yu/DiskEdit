# 02-incidents/inc-xfsprogs（102 行）

## 判断
- 裁剪 93：全部为依赖工具 xfsprogs（xfs_repair/mkfs.xfs/xfs_db/xfs_io/metadump/quota/logprint/libxfs）内部修复/特性；本实现仅按档案表以 xfs_growfs 承接在线扩容（SA2-004/005）及登记的 mkfs/attrs 面（SA2-012），工具内部随发行版版本固定（SA2-011）。

## 依据
- 参考行原文（xfsprogs CHANGES）均为上游程序内部行为；00055（mkfs.xfs 对齐自检）亦属上游内部，本实现 mkfs 固定参数调用。

## 撤回条件
- 若 xfs 检查（xfs_repair）或收缩面纳入档案表，重开对应行；依赖版本变更（SA2-011）时复查。

## 换版重放补充（2026-11-27，xfsprogs 6.6.0→7.3.0）

### 判断
- 裁剪 9（00001-00009）：doc/CHANGES 聚合块（healer/fiemap/xfs_scrub/mdrestore/fsr/mkfs/xfs_io/xfs_quota/xfs_repair/xfs_spaceman/xfs_db 等）；逐块核对均不含 xfs_growfs——本实现 xfs 面只登记 growfs（SA2-004/005），工具内部随发行版固定（SA2-011）。00001 重新锚定全批 note（旧锚行被重放覆盖）。

### 依据
- 参考行原文（doc/CHANGES:40/46/107/147/166/254/340/444/473）为上游各程序发布聚合。

### 撤回条件
- 若 xfs 检查（xfs_repair）或收缩面纳入档案表，重开对应行；依赖版本变更（SA2-011）时复查。
