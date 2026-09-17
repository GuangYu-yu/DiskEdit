# LICENSES

diskedit 本体：MIT（见 Cargo.toml `license`）

## 依赖许可证映射

| 依赖 | 版本 | 许可证 | 备注 |
|---|---|---|---|
| gptman | 3.1.1 | MIT OR Apache-2.0 | GPT 解析/提交 |
| crc | 3.4.0 | MIT OR Apache-2.0 | CRC32 |
| crc-catalog | 2.5.0 | MIT OR Apache-2.0 | crc 传递依赖 |
| fstool | 0.4.33 | MIT | 可选 browse feature（ls/cat） |
| libc | 0.2.189 | MIT OR Apache-2.0 | Linux 平台 |
| serde_json | 1.0.151 | MIT OR Apache-2.0 | dev-dependency |

本目录文本：MIT.txt、Apache-2.0.txt（标准全文）。

外部二进制工具（resize2fs/ntfsresize/mkfs.* 等）经 PATH 调用、不由本仓库
分发，许可义务归于其分发者。