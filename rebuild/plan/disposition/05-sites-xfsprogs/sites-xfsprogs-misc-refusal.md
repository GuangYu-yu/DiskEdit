# sites-xfsprogs-misc-refusal

## 判断
xfs_db 调试器命令面（25 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
sb/version/label/uuid 命令与 init 校验——本实现不消费 db 工具（档案表只登记 growfs），特性绑定不存在。

## 依据
crates/core/src/fs/catalog.rs、src/app/execute/filesystem.rs::verify_xfs_size。

## 撤回条件
本实现接入 xfs_db（如 UUID/标签改写改走 db）时重审。
