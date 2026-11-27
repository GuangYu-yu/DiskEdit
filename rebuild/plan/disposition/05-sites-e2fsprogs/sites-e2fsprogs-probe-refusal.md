# sites-e2fsprogs-probe-refusal

## 判断
dumpe2fs MMP/journal/badblocks 输出面拒绝与 -o 参数校验（12 行）。全批 12 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-003/SA2-011

## 撤回条件
引入 -o superblock/blocksize 等参数面时重审。
