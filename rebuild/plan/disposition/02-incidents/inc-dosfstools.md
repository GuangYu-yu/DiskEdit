# 02-incidents/inc-dosfstools（64 行）

## 判断
- 裁剪 64：全部为依赖工具 dosfstools（fsck.fat/mkfs.fat/fatlabel）内部修复/特性；本实现按档案表以 mkfs.vfat/fsck.vfat/fatlabel 承接（SA2-012），vfat 扩容按不支持拒绝（SA2-014），工具内部随发行版版本固定（SA2-011）。

## 依据
- 参考行原文（dosfstools ChangeLog）均为上游程序内部行为；00001（HDIO_GETGEO/几何起始读取）另涉 SA1-028 不实现几何。

## 撤回条件
- 若 vfat 调整（SA2-014）翻案支持，重开 FAT 修复相关行；依赖版本变更时复查。
