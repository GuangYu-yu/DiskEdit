# 判定行与拒绝文案基线

人读文本通常不作契约；以下两类除外。

## 1. 退出码语义（契约）

来源 `src/outcome.rs`（L20–L36）。七值，判定顺序即 `RunOutcome::exit_code`（L667–L680）的匹配顺序，命中即停：

| 码 | 变体 | 含义 |
| --- | --- | --- |
| 2 | `Usage` | 参数/命令语法错误；未写盘 |
| 1 | `Internal` | 未预期内部错误、不变量破坏 |
| 6 | `Unsupported` | 平台或能力不支持（工具缺失、无对应 API、目标类型不支持） |
| 5 | `Partial` | 写盘已发生但非整体成功；或已应用而内核视图 `Stale`；或记录已变而目标未全改 |
| 4 | `NoMutation` | 本次未改变任何持久状态（目标字节未写、记录条目未变）；目标忙也归此 |
| 3 | `Refused` | 策略拒绝或前置条件不满足（无表、无此分区、表非法） |
| 0 | `Checked` / `Applied` 且 `is_complete` | 完全成功 |

关键判据 `is_complete`（L657–L665）：`Checked`，或 `Applied { pending 空 且 kernel_sync==Synchronized }`。

证据类命中优先于准入拒绝：同一落点既有挂载又未完成记录时落 `5`，不改判 `3`。

## 2. `5` 的三要素（契约）

`5` 的输出必须含三样：目标可能已改变、证据路径、下一步命令（`inspect` / `recover` / 放弃回滚 之一）。来源：`src/cmd/inspect.rs`（L477–L577）给出的下一步命令文本，如 `diskedit recover ...`、`recover --release ...`。

## 3. `3` 的输出

不含证据路径与恢复动作。

## 4. 拒绝成因与固定下一步

来源 `src/fsops/capabilities.rs`：

- 扩容不适用时唯一出路文案 `CHANGE_PARTITION_ONLY`（L22）：`change the partition only with \`resize --no-fs\``。
- 缩容不接受 `--no-fs` 替身（L86–L89）。
- `rescue_hint`（L267–L289）按族给补救命令：ext `e2fsck -fp DEV && resize2fs DEV`；ntfs `ntfsresize -f -f DEV`；f2fs `fsck.f2fs DEV && resize.f2fs DEV`；xfs `mount DEV <mnt> && xfs_growfs <mnt>`；btrfs `mount ... && btrfs filesystem resize max <mnt>`（多设备带 `<devid>:max`）；vfat `fatresize -s max DEV`；swap `mkswap --uuid <uuid> DEV`。

## 5. 统一出口尾部

`src/outcome.rs` `NOTHING_WRITTEN`（L41）：`— this run changed no persistent state`，在 `Refused`/`NoMutation` 之后追加。
