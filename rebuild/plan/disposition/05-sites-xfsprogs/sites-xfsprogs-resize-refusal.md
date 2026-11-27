# sites-xfsprogs-resize-refusal

## 判断
xfs_growfs 拒绝面（22 行：7 认领 + 15 裁剪）。全批已逐行翻态，note 见同名 tsv。
- 认领 7 行（00007/08/12/13/16/29/42）：挂载点解析、不是已挂载 XFS 即拒绝（SA2-004 原型行为）、几何读取失败、data 段访问闸、FSGROWFSDATA/FSGEOMETRY 失败——调用时生效的校验/回读面。
- 裁剪 15 行（00018/20/21/23/25/27/30/31/32/35/38/39/40/41/44）：外部 log、realtime、inode pct、实验收缩、按段尺寸上限——本实现只发无参 grow 与 `-n` 回读，不在消费面。

## 依据
SA2-004/005；src/app/execute/filesystem.rs::online_invocation/run_online_native/verify_xfs_size；tests/online.rs::online_resize_refuses_shrinking_a_mounted_partition_with_a_non_btrfs_filesystem、an_xfs_grow_is_verified_by_the_data_geometry。

## 撤回条件
本实现引入选项段消费或 SA2-004/005 边界被推翻时重审。
