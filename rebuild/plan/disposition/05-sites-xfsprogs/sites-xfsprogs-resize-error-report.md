# sites-xfsprogs-resize-error-report

## 判断
xfs_growfs 消息面（10 行：4 认领 + 6 裁剪）。全批已逐行翻态，note 见同名 tsv。
- 认领 4 行（00009/10/26/28）：挂载点解析失败即退、目标确在 XFS 上、data 尺寸未变仍退 0、grow in progress 拒绝——调用时生效的校验/拒绝面，本实现不以退出码当成功、以 `-n` 几何回读兜住。
- 裁剪 6 行（00002/24/33/34/36/37）：usage 文本、实验收缩、realtime/log 段消息——本实现只发无参 grow 与 `-n` 回读，其余段不在消费面。

## 依据
SA2-004/005；src/app/execute/filesystem.rs::online_invocation/run_online_native/verify_xfs_size；tests/online.rs::an_xfs_grow_is_verified_by_the_data_geometry。

## 撤回条件
本实现引入 -D/-i/-l/-r/-m 选项段消费或 SA2-004/005 边界被推翻时重审。
