# sites-xfsprogs-mkfs-error-report

## 判断
mkfs.xfs 建卷过程消息（65 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
fprintf/fail 管道（含 proto 文件路径）随外部工具自带；本实现固定 `mkfs.xfs -f [-L label]` 调用，不复刻打印面、只消费退出码。

## 依据
src/app/execute/filesystem.rs::mkfs_invocation。

## 撤回条件
出现依赖 mkfs.xfs 过程文本的外部契约或 mkfs 参数面扩展时重审。

## 换版重放补充（xfsprogs 6.6.0→7.3.0）

换版重放在本批新增/改写 31 行（state=待判），已全部翻态：裁剪 31；note 见同名 tsv。
- 判断：新增/改写行仍属 mkfs.xfs 建卷过程消息（fprintf/fail 管道与 usage 出口，含上游新增的 rgsize/rgcount、atomic write、autofsck、zoned 约束告警），随外部工具自带；固定调用只消费退出码。
- 依据：src/app/execute/filesystem.rs::mkfs_invocation。
- 撤回条件：出现依赖 mkfs.xfs 过程文本的外部契约或 mkfs 参数面扩展时重审。
