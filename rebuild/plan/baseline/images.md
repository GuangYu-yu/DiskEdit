# 固定镜像配方

镜像本身不入库（`.gitignore` 覆盖 `plan/baseline/images/` 与 `**/*.img`）；只记生成方式。生成脚本随 `rebuild/tests` 交付，作为用例表的 `Recipe` 数据。

## 目标形态四类

| 形态 | 构造 | 需要特权 |
| --- | --- | --- |
| 镜像文件 | 普通文件，经 loop 映射或直接按偏移读写 | 否（直接文件）/ 是（loop） |
| loop 设备 | `losetup` 绑定镜像或其分区 | 是 |
| 块设备整盘 | 真实盘或 `scsi_debug` 等 | 是 |
| 块设备分区 | 整盘的 `/dev/sdXN` | 是 |

## 表类型

| 配方 | 构造 |
| --- | --- |
| GPT 空表 | `new --table gpt --yes` |
| MBR 空表 | `new --table mbr --yes` |
| 无表 | 全零镜像 |
| GPT 含多分区 | `new` + 多次 `add` |
| MBR 含扩展分区 | `new` + 逻辑分区链 |

## 文件系统（每类至少一个）

ext2/3/4、xfs、btrfs、f2fs、vfat、exfat、ntfs、swap 各一；squashfs、erofs、hfsplus、apfs、bcachefs、jfs、reiserfs、udf 至少一个只读识别样本。

## 损坏样本

- GPT 主头 CRC 错、备份头缺失、保护 MBR 异常。
- MBR 签名缺失、扩展链断裂。
- 记录：头 CRC 错、载荷截断、外来 format id。

配方名与用例 `id` 对应，失败消息打印配方名与可复现命令。
