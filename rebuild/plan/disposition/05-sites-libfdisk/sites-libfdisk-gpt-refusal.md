# sites libfdisk gpt refusal 处置说明

差异登记：fdisk verify 的逐项 + N errors 汇总诊断未复刻，本实现以 info/inspect 损伤类别与退出码呈现（判定逻辑本身已认领）。
D-建议:如需人工逐项诊断，可在 info 损伤字段附明细列表。
撤回条件：出现依赖逐项 verify 清单的外部契约（脚本解析 verify 文本）时重评。
