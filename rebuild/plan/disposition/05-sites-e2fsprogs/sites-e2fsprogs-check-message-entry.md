# sites-e2fsprogs-check-message-entry

## 判断
e2fsck problem 表 311 行：消息、PROMPT_* 交互应答、fix_problem 判读。全批 311 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001/002（不做 fs 修复，只消费退出码）；交互面裁剪（e2fsck -f -n / -fp 非交互，src/platform/linux/tools.rs::check_arguments）；SA2-001 定次序

## 撤回条件
本实现引入任何 fs 修复能力，或出现必须复刻 problem 表文案/交互应答的外部契约（须先按 §10 登记）时重审。
