# sites-btrfs-progs-label-uuid-refusal

## 判断
btrfstune 拒绝面（7 行：5 认领 + 2 裁剪）。全批已逐行翻态，note 见同名 tsv。
- 认领 5 行（00492/94/502/504/506）：change-uuid.c 的 fsid 变更拒绝与失败路径——本实现 UUID 写入固定 `btrfstune -f -U <uuid>` / `-f -u`（uuid_program(Btrfs)=Btrfstune，crates/core/src/fs/attrs.rs；src/app/execute/filesystem.rs content_property），调用时生效、退出码消费。
- 裁剪 2 行（00489/490）：change-metadata-uuid.c 的 -m metadata UUID 特性——本实现只用 -U/-u，metadata UUID 面不在消费范围。

## 依据
crates/core/src/fs/attrs.rs::uuid_program/uuid_support（Btrfs=ExplicitAndGenerated）；src/app/execute/filesystem.rs content_property（btrfstune -f -U/-u）。

## 撤回条件
本实现接入 -m metadata UUID 或 btrfstune 改参时重审裁剪行；UUID 写入边界变化时重审视认领行。
