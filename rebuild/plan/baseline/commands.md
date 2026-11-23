# 命令面基线

来源：`src/cli.rs` `COMMANDS`（L634–L1044）。`diskedit {name} {syntax}` 形式。目标后缀 `:N`（1..=128）或 `:last`，见 `src/dev/target.rs`（L5–L52）；`:0` 拒绝。值域来自 `src/cli.rs` `ValueDomain`（L38–L67）。

## 公共约定

- 所有写目标字节的命令：`confirmation = Required`，必须有 `--yes`；只读命令 `confirmation = None`，不接受 `--yes`。
- `--sector-size` 值域 `{512,1024,2048,4096,8192,16384,32768,65536}`；集合外落退出码 `2`。
- `--identity` 指定目标身份；`--chunk` 默认 4 MiB（续跑沿用记录值）。

| # | 命令 | 分组 | 目标后缀 | 写目标 | 需 --yes | 形态（syntax） | 位置槽 | 专属选项 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | `info` | Everyday | Whole / Partition | 否 | 否 | `<TARGET> [--sector-size N]` | TARGET | — |
| 1b | `info` | | Partition | 否 | 否 | `<TARGET>:N --grow [--sector-size N]` | TARGET | `--grow`（选择器） |
| 2 | `inspect` | Everyday | Whole | 否 | 否 | `<TARGET\|PATH>` | TARGET | — |
| 3 | `records list` | Everyday | Whole | 否 | 否 | `[TARGET]` | 可选 TARGET | — |
| 4 | `records verify` | Everyday | Whole | 否 | 否 | `<TARGET> <ID>` | TARGET, ID(32-hex) | — |
| 5 | `records scan` | Everyday | Any | 否 | 否 | `<ROOT>` | ROOT | — |
| 6 | `resize` | Everyday | Any | 是 | 是 | `<TARGET>[:N] <SIZE> [--allow-move] [--chunk N] [--grow-lv] [--lv NAME] [--no-fs] --yes` | TARGET, SIZE | `--allow-move --chunk --dry-run --grow-lv --lv` |
| 6b | `resize` | | Partition | 是 | 是 | `<TARGET>:N --end LBA [--align A] [--no-fs] --yes` | TARGET | `--dry-run --end --align` |
| 6c | `resize` | | Partition | 是 | 是 | `<TARGET>:N --grow-to-end [--align A] [--no-fs] --yes` | TARGET | `--dry-run --grow-to-end --align` |
| 7 | `move` | Everyday | Partition | 是 | 是 | `<TARGET>:N --start <LBA\|end\|align> [--chunk N] --yes [--align A]` | TARGET | `--align --chunk --start` |
| 8 | `copy` | Everyday | Partition | 是 | 是 | `<TARGET>:N --start <LBA\|end> [--chunk N] --yes [--align A] [--name S] [--keep-partuuid]` | TARGET | `--align --chunk --keep-partuuid --name --start` |
| 9 | `delete` | Everyday | Partition | 是 | 是 | `<TARGET>:N --yes` | TARGET | — |
| 10 | `set` | Everyday | Partition | 是 | 是 | `<TARGET>:N name S --yes` | TARGET, PROPERTY, VALUE | 选择器 `PROPERTY=name` |
| 10b | `set` | | Partition | 是 | 是 | `<TARGET>:N label S --yes` | TARGET, PROPERTY, VALUE | 选择器 `PROPERTY=label` |
| 10c | `set` | | Partition | 是 | 是 | `<TARGET>:N uuid [U] --yes [--random]` | TARGET, PROPERTY, 可选 VALUE | 选择器 `PROPERTY=uuid`；`--random` |
| 10d | `set` | | Partition | 是 | 是 | `<TARGET>:N flag F on\|off --yes` | TARGET, PROPERTY, VALUE, STATE | 选择器 `PROPERTY=flag` |
| 10e | `set` | | Partition | 是 | 是 | `<TARGET>:N type GUID\|0xXX --yes` | TARGET, PROPERTY, VALUE | 选择器 `PROPERTY=type` |
| 11 | `check` | Everyday | Partition | 否 | 否 | `<TARGET>:N` | TARGET | — |
| 12 | `mkfs` | Everyday | Partition | 是 | 是 | `<TARGET>:N <FS> --yes [--label S]` | TARGET, FS | `--label` |
| 13 | `resizefs` | Everyday | Partition / Whole | 是 | 是 | `<TARGET>:N [BYTES] --yes` | TARGET, 可选 BYTES | — |
| 13b | `resizefs` | | Whole | 是 | 是 | `<MOUNTPOINT> [BYTES] --yes` | TARGET, 可选 BYTES | — |
| 14 | `recover` | Everyday | Whole | 是（仅记录侧） | 是 | `<TARGET> --yes` | TARGET | — |
| 14b | `recover` | | Whole | 是 | 是 | `<TARGET> --replay-table-only --yes` | TARGET | `--replay-table-only` |
| 14c | `recover` | | Whole | 是 | 是 | `<TARGET> --release [--discard-replay] --yes` | TARGET | `--release --discard-replay`（`--discard-replay` requires `--release`） |
| 15 | `new` | LowLevel | Whole | 是 | 是 | `<TARGET> [--table gpt\|mbr] --yes [--overwrite-data]` | TARGET | `--table --overwrite-data` |
| 16 | `add` | LowLevel | Whole | 是 | 是 | `<TARGET> SIZE [--start LBA] [--align A] [--part-number N] [--name S] [--type-guid G] --yes` | TARGET, SIZE | `--align --name --part-number --start --type-guid` |

## 值域与互斥

- `--table`：`gpt` \| `mbr`（`recover`/`resize` 等不接受）。
- `--flag` 的 `F` 与 `--type` 的 `GUID|0xXX`：由 `set` 的位置槽校验；非法落 `2`。
- `--chunk N`：字节数或尺寸后缀，须为正；`move`/`copy` 续跑时沿用记录值。
- `--end LBA`、`--grow-to-end`：二者互斥（选择器各占一形态）；`--align` 仅这两形态与 `add`/`move`/`copy` 接受。
- `--discard-replay` 仅在 `recover --release` 形态出现，且 `requires --release`。
- `--no-fs` 等价 `resize --no-fs`；`resizefs` 不声明该选项。

## 形态判定（选择器）

- 依据旗标：`--grow`（info）、`--end`（resize）、`--grow-to-end`（resize）、`--replay-table-only`/`--release`（recover）。
- 依据位置字面量（`set`）：`PROPERTY ∈ {name,label,uuid,flag,type}`。
- `resizefs`：目标带 `:N` 与不带（挂载点）各一形态。
- 目标后缀约束：`Whole` 禁止 `:N`；`Partition` 要求 `:N`/`:last`；`Any` 二者皆可。

## 用法错误 → 退出码 2

`src/cli.rs` 解析流程（L1779–L1963）：未知命令、未知/未授权选项、缺参数、多余参数、重复选项、值域不符、互斥/`requires` 不满足、缺 `--yes`，全部经 `bail_fail(RunOutcome::usage(...))` 落 `2`。
