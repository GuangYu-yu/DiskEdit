# sites-btrfs-progs-frontend-refusal

## 判断
cmds 层拒绝面（84 行：12 认领 + 72 裁剪）。全批已逐行翻态，note 见同名 tsv。
- 认领 12 行（00032/33/35/42/44/48-54）：check_resize_args/cmd_filesystem_resize 的参数校验与错误路径——resize 参数面（fs info、尺寸串校验、收缩越界即拒、mounted-only、独占操作检查、执行失败报错），本实现按 SA2-006/007/008/016 消费。
- 裁剪 3 行（00038/40/46）：devid 前缀/按增量扩缩形态——本实现只发 `max` 与 `<N>k`（单设备目标），多设备在判定层按占用拒绝（B17）。
- 裁剪 69 行：df/show/defrag/mkswapfile/usage/property/device 级子命令拒绝面——绑定未消费子命令。

## 依据
SA2-006/007/008/016；src/app/execute/filesystem.rs::online_invocation/grow_plan/shrink/verify_btrfs_size；tests/online.rs 三条 btrfs 用例；plan/06-防漂移执行手册.md B17。

## 撤回条件
本实现引入 devid/多设备 resize 或按增量扩缩时重审裁剪行；SA2-006/007/008/016 边界被推翻时重审视认领行。
