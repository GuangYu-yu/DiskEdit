# sites-e2fsprogs-label-uuid-string-channel

## 判断
tune2fs metadata_csum 校验重写失败打印（1 行）。全批 1 行已逐行翻态，note 见同名 tsv。

## 依据
命令面冻结；src/app/execute/filesystem.rs::uuid_verdict

## 撤回条件
引入特性改写面时重审。
