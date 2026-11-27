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

## 换版重放补充（btrfs-progs 6.6.3→7.3）

换版重放在本批新增/改写 36 行（state=待判），已全部翻态：认领 3 / 裁剪 33；note 见同名 tsv。
- 判断：新增行两类——在线 resize 消费面（00033/00035 尺寸串校验与解析、00061 独占操作检查错误路径）认领（SA2-006/007/008/016 + src/app/execute/filesystem.rs::btrfs_size/grow_plan/shrink/online_invocation）；上游 7.x 新增的 --offline 离线扩容（00036-00046）、--enqueue（00058）、镜像文件 resize（00060）、devid 形态（00034/00052）、按增量扩缩（00054/00056）与 device/property/mkswapfile/commit-stats 子命令面判裁剪——本实现只发 `max` 与绝对目标 `<N>k`、走挂载后在线 resize。
- 依据：SA2-006/007/008/016；src/app/execute/filesystem.rs；tests/online.rs 三条 btrfs 用例；plan/06-防漂移执行手册.md B17。
- 撤回条件：本实现引入 --offline/--enqueue/devid/按增量形态或上述子命令消费时重审；SA2-006/007/008/016 边界被推翻时重审视认领行。

## 口径定案（在线收缩悬案，2026-11-27 关闭）

曾有旧口径认为"基线拒绝 btrfs 在线收缩、实现却做了"。换版重放后核实：上游 btrfs-progs 本身即要求挂载后 resize（离线形态 7.x 才以 --offline 新增，判裁剪），在线收缩是基线**既有行为**；本实现同样走挂载后 resize（SA2-007/016，真机 s22 实测），实现与基线一致，属认领——**不构成差异，不进 D 表**。现行账本无任何"基线拒绝在线收缩"的行；本节为终局记录，防重翻案。
