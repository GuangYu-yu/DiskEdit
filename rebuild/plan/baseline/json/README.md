# 机器可读输出基线

字段名、类型与取值语义是契约（`01-目标与边界.md` §3.3）。以下从源码构造点读出；实跑样本记「未执行」，由 `rebuild/tests` 契约用例补齐。

## `info --json`

来源 `src/cmd/info.rs`（L140–L260）。顶层键集在 GPT 与 MBR 两个分支一致：

| 字段 | 类型 | 语义 |
| --- | --- | --- |
| `table` | `"gpt"` \| `"mbr"` | 表类型 |
| `damaged` | bool | GPT：非 `Valid`、保护 MBR 非 `Normal`、或条目重叠；MBR：`raw.damage` 非空 |
| `sector_size` | number | 扇区字节数 |
| `size_bytes` | number | 目标字节数 |
| `disk_guid` | string hex \| null | GPT 磁盘 GUID；MBR 为 `null` |
| `last_part` | number（可选） | `:last` 指向的条目号；无已用条目则缺字段 |
| `partitions` | array | 条目数组（跳过未用条目） |
| `error` | string | 识别/读失败时追加；失败仍输出已读出的布局 JSON |

`partitions[]` 条目键集两分支一致：

| 字段 | 类型 | 语义 |
| --- | --- | --- |
| `num` | number | 分区号（1 起） |
| `first_lba` | number | 起始 LBA |
| `last_lba` | number | 结束 LBA（含） |
| `size_bytes` | number | 区间字节数 |
| `type` | string | GPT：类型 GUID hex；MBR：`"0xNN"` |
| `fs` | string | 识别名；容器条目 `"container"`；识别失败 `"error"` |
| `name` | string \| null | GPT 分区名；MBR 为 `null` |

## `records list --json`

来源 `src/cmd/records.rs` `Row::json`（L155–L179）。每行一个对象：

| 字段 | 类型 | 语义 |
| --- | --- | --- |
| `id` | string 32-hex \| null | 事务 id |
| `path` | string | 证据路径 |
| `backend` | string | 记录后端 |
| `state` | string \| null | `valid` / `unfinished` / `identity_mismatch` / `unknown_format` / `damaged` / `unreadable` / `backend_mismatch` |
| `quarantine` | string \| null | 隔离标记 |
| `op` | string \| null | 操作命令名 |
| `ranges` | number \| null | 被覆盖区间条数 |
| `commit` | `"prepared"` \| `"committed"` \| null | 提交态 |
| `retention` | string \| null | 保留态 |
| `identity_check` | `"checked"` \| `"unavailable"` | 身份核对结论 |
| `registered` | bool | 固定 `false` |

## `records verify --json`、`inspect --json`、`check --json`

- `records verify`：命中按记录状态返回；`unverified`/`not_found` 输出 JSON 并落 `3`。来源 `src/cmd/records.rs`（L593–L674）。
- `inspect`：判定行与下一步指引见 `judgment-lines.md`；字段构造在 `src/cmd/inspect.rs`（L477–L577）。
- `check`：结构化检查结论；字段来源待实跑采集成样本（记「未执行」）。

以上三命令的逐字段样本需运行采集，列入 `05-静态自检.md` 未执行清单。
