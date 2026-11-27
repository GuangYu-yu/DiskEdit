# sites-e2fsprogs-check-refusal

## 判断
e2fsck 内部拒绝、problem 表字符串与 fix_problem 站点（271 行）。全批 271 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001/002；B4/B17 占用闸；SA1-002 只读

## 撤回条件
同 message-entry 批；另：若占用闸放宽到允许写占用中目标，check_mount 场景需重审。
