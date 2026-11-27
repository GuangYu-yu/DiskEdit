# 03-docs/docs-gdisk（38 行）

## 判断
- 裁剪 38：BUGS 节全部为——节标题/文档元条目、平台面（D10 非 Linux 后端）、MBR→GPT/BSD disklabel 转换（SA1-016、转换特性不实现）、混合 MBR 引导面（SA3-002）、fixparts 工具、交互 TUI 显示面（D9 JSON 无列宽）、Windows 名称限制。
- 等价防线在行 note 注明：挂载写入限制→SA1-004 占用闸；名称 UTF-16→GPT-006；条目 128 下限→GPT-020。

## 依据
- 参考行原文（gdisk/cgdisk/fixparts man BUGS）均为平台/特性限制，本实现限制面由各 note 引的 SA/代码承接。

## 撤回条件
- 若新增转换（MBR→GPT、BSD）、fixparts、混合 MBR 写面，重开对应行。
