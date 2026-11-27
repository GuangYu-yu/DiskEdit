# 02-incidents/inc-testdisk（61 行）

## 判断
- 裁剪 61：testdisk 为恢复参考工具；本实现仅借其 GPT 恢复语义（SA3-004：主备任一可读即修复、不猜 GUID、PMBR 写前拒绝），不做全盘搜索/文件雕刻/文件列表/TUI（SA1-016/SA1-017）；其溢出/崩溃修复属 C 实现且代码不在本实现内。

## 依据
- 参考行原文（testdisk ChangeLog）均为 PhotoRec/文件系统探测/TUI 行为；CHS/几何提示行另涉 SA1-028；RAID 信息行等价防线是占用探测沿 holders（SA1-025）。

## 撤回条件
- 若引入全盘搜索恢复或文件雕刻面，重开对应行。
