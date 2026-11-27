# sites-xfsprogs-resize-fatal-exit

## 判断
xfs_growfs 出口（13 行：4 认领 + 9 裁剪）。全批已逐行翻态，note 见同名 tsv。
- 认领 4 行（00011/14/43/45）：目标/几何校验失败的退出码产出端、新旧几何 datablocks 比对、最终 exit(error)——run_online_native 消费 0/非零/信号三态。
- 裁剪 9 行（00001/03/04/05/06/15/17/19/22）：usage、旗标组合、外部 log/realtime 段出口——不在消费面。

## 依据
SA2-004/005；src/app/execute/filesystem.rs::run_online_native/verify_xfs_size。

## 撤回条件
同 resize-error-report：选项段消费或 SA2 边界变化时重审。

## 换版重放补充（xfsprogs 6.6.0→7.3.0）

换版重放在本批新增/改写 1 行（state=待判），已全部翻态：裁剪 1；note 见同名 tsv。
- 判断：新增行为 -r realtime 段出口（failed to access realtime device）——本实现只发无参 grow（`xfs_growfs <mount>`）与 `-n` 回读，realtime 段不在消费面。
- 依据：SA2-004/005；src/app/execute/filesystem.rs::online_invocation/verify_xfs_size。
- 撤回条件：本实现引入 -r/-l 等选项段消费或 SA2-004/005 边界被推翻时重审。
