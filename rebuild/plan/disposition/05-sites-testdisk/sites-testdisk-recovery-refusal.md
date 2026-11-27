# 处置说明：testdisk recovery refusal（GPT 头/表校验）

- 批次：`05-sites-testdisk/sites-testdisk-recovery-refusal`（参考账本 sites/testdisk）

## 判断

GPT 头签名/尺寸/CRC/LBA/条目校验、只读告警、条目不合法拒绝全部认领（SA1-002/SA3-003/SA3-004）；RAID/NTFS/ReFS 内容校验裁剪（SA1-017）。

## 依据

SA1-002/SA1-017/SA3-003/SA3-004；crates/core/src/table/gpt/parse.rs。

## 撤回条件

撤回条件：若引入内容级识别（RAID/LVM 等），先补档案表探针与拒绝矩阵（SA1-017 同款）。
