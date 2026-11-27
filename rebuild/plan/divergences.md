# 差异登记

与基线的刻意差异只允许下列条目，每条必须有测试与撤回条件。其余任何行为差异按未登记处理，等同未通过。

| # | 行为 | 理由 | 撤回条件 | 测试位置 |
| --- | --- | --- | --- | --- |
| D1 | 写盘之后不可能得出"本次未改动" | 拒绝只发生在未写状态 | 出现必须在写后报告"未改动"的真实场景 | `src/app/execute/state.rs` 的状态机用例、`tests/case_runner.rs` 的拒侧用例 |
| D2 | 非本实现写下的记录视为外来，拒绝恢复。记录格式换代：不读取、不迁移旧格式。目标上留有旧版在途记录时，本实现的写命令一律拒绝且不改目标字节；该记录只能由写下它的旧版收尾，或由人确认放弃 | 记录是崩溃恢复的中间物，不是用户数据；本实现不留任何旧格式解码路径 | 确认存在必须现场恢复、且无法用写它的版本收尾的记录 | `crates/core/src/record/{codec,recover}.rs` 的外来格式用例；`tests/cases` 的“旧版在途记录在场时写命令被拒、目标字节不变”用例 |
| D3 | 检查器原文输出与失败原因可见 | 结论必须可诊断 | 输出体积成为实际障碍且有替代证据 | `src/app/decide/check.rs` 与 `check` 用例 |
| D4 | 需修复才能判定的操作先修复再判定；修复写入目标后失败一律部分完成（`5`） | 避免修复自身改变判定前提 | 修复被证明不写目标字节 | **未验证**：计划中的“脏 ext 收缩”夹具用例不存在；当前只有表修复用例 `tests/gpt_repair.rs`，文件系统修复路径没有故障注入 |
| D5 | 退出码只由终态映射 | 消除调用点自行选码 | 出现终态无法表达的对外契约 | `src/app/report/exit.rs` 的码表用例 |
| D6 | 缺失与空集合不折叠为默认值 | 未观测不等于没有 | 出现必须把未观测当默认值的外部契约 | `tests/case_runner.rs` 的未知事实拒侧用例、`Observation` 用例 |
| D7 | 载体路径与基线行为等价 | 等价指行为一致，不按可运行性裁剪 | 基线载体在目标环境可运行而本实现不能 | `tests/carrier.rs` 两条：来宾在线扩容（宿主回读超级块）与 `write_disk` 整盘偏移写入；需设置 `DISKEDIT_VM_BUNDLE`，未设置时会打印 `NotApplicable` 并被 `cargo test` 计为通过，因此交付记录必须写明 bundle 已配置 |
| D8 | 不因几何变化改写用户已有的分区类型 GUID | 类型 GUID 是用户数据 | 确认基线确实改写且外部依赖该改写 | `tests/case_runner.rs::move_and_copy_relocate_data_and_keep_type_guids`（断言搬移后两处类型 GUID 不变） |
| D9 | 只读命令统一输出本实现的结构化 JSON；`inspect`/`check` 的人读文本与基线不逐字复刻 | 基线实际只有 `info` 与 `records *` 输出 JSON；`inspect`/`check` 输出人读文本且无 `--json`，人读文本不是契约 | 出现依赖基线文本措辞或基线 JSON 字段名的外部契约 | `tests/equivalence.rs::both_binaries_agree_on_info_json_fields`（8 组输入按解析后的 JSON 值对照）；`tests/case_runner.rs::read_only_info_emits_one_structured_json_document` |
| D10 | 本期只交付 Linux 后端；Windows/macOS 宿主的表编辑与文件系统操作返回 `NotSupported`（退出码 `6`） | `01` §7.4 排除非 Linux 后端；端口形状已留出，补齐时只加子树 | Windows/macOS 上的等价表编辑成为交付条件 | **部分未验证**：非 Linux 后端无法在本机构建；`src/platform/mod.rs` 的 `unsupported_platform!` 只有拒绝码用例，计划中的 `tests/generated_docs.rs` 不存在 |
| D11 | 块设备目标在缺少 `CAP_SYS_ADMIN` 的宿主上改由虚拟机载体执行；无可用载体时以 `6` 拒绝，而不是照常尝试并得到内核 `EPERM` | 分区设备视图与内核重读在裁剪过能力的容器里被禁用，照常执行只会得到写前失败；普通文件目标经 loop 得到分区视图，不受该能力限制，仍在宿主直接执行 | 宿主能力探测改为逐项验证 loop/BLKPG/BLKRRPART，且不再依赖 euid 与 `CapEff` | `src/platform/linux/mod.rs::filesystem_ready`、`tests/case_runner.rs` 的块设备用例 |
| D12 | 退出码 `5` 的人读输出在三要素之后追加一句具体原因 | 原因原先在报告层被丢弃，用户拿不到排障线索；三要素文本本身不变 | 输出体积或逐字稳定性成为对外契约 | `src/app/report/text.rs` 的 `recovery_text_keeps_the_three_elements_and_adds_the_cause` |
| D13 | 重写 PMBR 时保留 0..446（引导代码+磁盘签名），不按 UEFI 建议清零重建 | 引导代码与签名是用户/固件数据，抹除会造成无谓破坏；UEFI 条款允许固件忽略这些字节 | 出现必须清零签名/引导代码的外部契约 | `crates/core/src/table/create.rs::protective_mbr_is_rewritten_only_when_non_canonical` |
| D14 | 小数尺寸按精确字节分数换算再对齐（9.5G=9.5×10^9 字节），不截断到整数单位 | 截断会静默缩小用户请求的容量；gdisk 曾因此出错（gdisk-inc-00002） | 出现要求按整数单位截断解释的外部契约 | `src/app/cli/parse.rs::absolute_size` 的分数换算（待补独立测试锚） |
| D15 | ext/NTFS/vfat 卷标超长时在写任何字节前按字段容量拒绝（ext 16 字节、NTFS 128 UTF-16 码元、vfat 11 字符），不采纳参考工具的"截断+警告" | 截断静默改写用户数据；前置拒绝与"写前拒绝"架构一致 | 出现必须接受并截断卷标的外部契约 | `src/app/decide` 卷标校验（`attrs.rs::label_problem` 等；**待锚**：三族各补一条拒绝单测） |
| D16 | `info`/`inspect` 报损伤类别与退出码，不复刻 libfdisk `verify` 的逐项 N errors 清单 | D9 的延伸：结构化输出是契约，人读清单不是；类别+退出码已可诊断 | 出现必须逐项列出错误清单的外部契约 | `tests/equivalence.rs::both_binaries_agree_on_info_json_fields` |
| D17 | EBR 链任一级损坏时整体读失败拒绝，不复刻 libfdisk 对链尾损坏"警告并跳过"的宽容读 | 宽容跳过会用不完整数据继续写表；"无法自洽的表按拒绝处理"（SA3-004 同族） | 出现必须对损坏链尾降级续读的真实场景 | `crates/core/src/table/mbr/extended.rs` 的错误路径（待补链尾损坏单测） |
| D18 | 不复刻 fdisk/cfdisk 的列式人读表格（scols），结构化输出为 `info` JSON | D9 同族：人读表格不是契约 | 出现依赖列式表格布局的外部契约 | `tests/equivalence.rs` |

本表是差异的唯一来源：行为、理由、撤回条件、测试位置四项只在这里维护。`01-目标与边界.md` 第 5 节只摘录行为一句并指向本表，不另写撤回条件与测试位置。新增差异只改本表；未经本表登记的行为差异一律按未通过计。

## 登记资格

- 每条降级、收窄或“不支持”，必须引用 `baseline/` 里基线证据的具体条目，证明基线在该形态下确实没有可复现的行为。引不出证据的，写进 `05-静态自检.md` 的未执行清单，不得进入本表。
- 基线能做、本实现没做的，是缺口，不是差异。
