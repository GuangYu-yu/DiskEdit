# sites-e2fsprogs-probe-error-report

## 判断
dumpe2fs 全量输出与内部消息（19 行）。全批 18 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-003/SA2-011（只解析 Block count/Block size 稳定字段，缺字段即拒绝）；SA2-002

## 撤回条件
回读字段解析变化（版本漂移，SA2-011 撤回条件）或引入全量输出依赖时重审。
