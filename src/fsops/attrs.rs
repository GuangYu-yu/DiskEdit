//! FS 属性操作：check、label 与 UUID。

use super::*;

/// FS 一致性检查（不修改分区边界）。各类型的工具与开关见下方匹配臂
pub fn check_fs(src: &FileSource, part: u32, fstype: Option<ContentKind>) -> Result<(), FsError> {
    if fstype == Some(ContentKind::Lvm2Pv) {
        return Err(FsError::invalid(
            "target is an LVM2 PV, not a filesystem — PV metadata is checked with pvck(8), not fsck",
        ));
    }
    // 访问方式与工具同表：`fsck.f2fs` 无参也可能写回全局元数据（见上一条），这一臂
    // 按"可能写"声明；其余各臂都带明确的只读开关
    let Some(check) = Check::for_fstype(fstype) else {
        return Err(FsError::unsupported(format!(
            "no check tool wired for {}",
            content_name(fstype.unwrap_or(ContentKind::Unknown))
        )));
    };
    with_partition_device(src, part, |dev| check.run(dev))
}

/// VFAT 卷标的准入判据。字段容量是 11 **字节**（man fatlabel），而 VFAT 按 DOS/OEM
/// 代码页存取标签，所以判据必须是编码后的字节数。本工具不做代码页转换（无编码库依赖），
/// 前置准入因此只收 ASCII：ASCII 每字符恰好一字节，`len()` 即编码后的长度；非 ASCII
/// 标签在本层拒绝。这是本工具自身的限制，不是字段的限制——fatlabel 支持按代码页
/// 设置非 ASCII 标签，需要时直接用 fatlabel
pub(crate) fn vfat_label_check(label: &str) -> Result<(), FsError> {
    if !label.is_ascii() {
        return Err(FsError::invalid(format!(
            "vfat label {label:?} cannot be represented in a DOS code page by this tool \
             (labels here are accepted as ASCII only); set it with fatlabel directly"
        )));
    }
    if label.len() > 11 {
        return Err(FsError::invalid(format!(
            "vfat label exceeds the 11-byte on-disk limit (got {} bytes); fatlabel would reject it",
            label.len()
        )));
    }
    Ok(())
}

/// 卷标落在定长字段里的类型按字段容量前置校验，超出即拒绝——截断与拒绝是两种结局，
/// 本层不把它们合并成一次"设置成功"。`set label` 与 `mkfs --label` 共用这一份判据：
/// 建 FS 时才写卷标的话，判据必须同样发生在盘上第一个字节被擦除之前。
/// 来源：tune2fs -L（man tune2fs，ext 超级块 `s_volume_name[16]`）、
/// xfs_admin -L ≤12 字节（man xfs_admin）、
/// btrfs 超级块 label（含结尾 NUL 共 256 字节）、ntfslabel 的 128 个 UTF-16 码元字段、
/// fatlabel 的 11 字节字段（man fatlabel）、exfatlabel 的 11 个 UTF-16 码元字段、
/// swaplabel -L 的 volume_name[16]（xstrncpy 至多 15 字节）
pub fn check_label_limits(fstype: Option<ContentKind>, label: &str) -> Result<(), FsError> {
    match fstype {
        // ext 超级块卷标字段 s_volume_name[16] 按字节存放，不要求结尾 NUL：16 字节填满
        // 字段是合法值，超出即在本层拒绝，不把判决留给 tune2fs 的参数错误
        Some(ContentKind::Filesystem(FsKind::Ext)) => {
            if label.len() > 16 {
                return Err(FsError::invalid(format!(
                    "ext label exceeds the 16-byte s_volume_name field (got {} bytes); \
                     the kernel and e2fsprogs read at most 16",
                    label.len()
                )));
            }
        }
        // XFS 标签上限 12 字节（superblock s_fname[12]，man xfs_admin "twelve
        // characters"）：超长时 xfs_admin 静默截断，故提前拒绝
        Some(ContentKind::Filesystem(FsKind::Xfs)) => {
            if label.len() > 12 {
                return Err(FsError::invalid(format!(
                    "xfs label exceeds 12 bytes (got {}); the on-disk field holds 12",
                    label.len()
                )));
            }
        }
        // 超级块 label 含结尾 NUL 共 256 字节，超出时 btrfs 拒绝，故提前按字节拒绝
        Some(ContentKind::Filesystem(FsKind::Btrfs)) => {
            if label.len() > 255 {
                return Err(FsError::invalid(format!(
                    "btrfs label exceeds 255 bytes (got {}); btrfs would reject it",
                    label.len()
                )));
            }
        }
        // 卷标字段 128 个 UTF-16 码元，超出时 ntfslabel 截断，故提前拒绝
        Some(ContentKind::Filesystem(FsKind::Ntfs)) => {
            let units = label.encode_utf16().count();
            if units > 128 {
                return Err(FsError::invalid(format!(
                    "ntfs label exceeds 128 UTF-16 units (got {units}); ntfslabel would truncate it"
                )));
            }
        }
        // 卷标字段 11 字节，判据见 vfat_label_check
        Some(ContentKind::Filesystem(FsKind::Vfat)) => vfat_label_check(label)?,
        // 卷标字段 11 个 UTF-16 码元，超出时 exfatlabel 以 E2BIG 拒绝，故提前拒绝
        Some(ContentKind::Filesystem(FsKind::Exfat)) => {
            let units = label.encode_utf16().count();
            if units > 11 {
                return Err(FsError::invalid(format!(
                    "exfat label exceeds 11 UTF-16 units (got {units}); exfatlabel would reject it"
                )));
            }
        }
        // volume_name[16] 留结尾 NUL，xstrncpy 至多复制 n-1 字节，故 15 字节是上限；
        // 超出时 swaplabel 截断，故提前按字节拒绝
        Some(ContentKind::Swap) if label.len() > 15 => {
            return Err(FsError::invalid(format!(
                "swap label exceeds 15 bytes (got {}); swaplabel would truncate it",
                label.len()
            )));
        }
        _ => {}
    }
    Ok(())
}

/// 设置 FS label。值原样传给各 FS 官方工具；字段容量判据见 [`check_label_limits`]
pub fn set_label(
    src: &mut crate::dev::WriteSource,
    part: u32,
    fstype: Option<ContentKind>,
    label: &str,
) -> Result<(), FsError> {
    check_label_limits(fstype, label)?;
    let (device, _) = partition_device(src, part)?;
    let dev = device.path();
    let Some(label_op) = Label::for_fstype(fstype) else {
        return Err(FsError::unsupported(format!(
            "no label tool wired for {}",
            content_name(fstype.unwrap_or(ContentKind::Unknown))
        )));
    };
    // 工具缺失时一个字节都不写：存在性确认也在屏障之前
    label_op.tool().require_named("set the label")?;
    // 屏障紧贴工具启动：设备已建立、工具已确认，之后的成败都可能改过目标
    src.mark_non_reversible()
        .map_err(|e| FsError::from(io::Error::other(e.record_failure("the transaction barrier"))))?;
    let out = label_op.write(dev, label)?;
    if !out.status.success() {
        return Err(FsError::command(label_op.tool().name(), &out));
    }
    Ok(())
}

/// 一次 UUID 设置请求的两种形式。**不含"目标能不能接受"**——那是 [`uuid_support`] 回答的，
/// 由调用方比对后决定拒还是做（命令层是唯一能给出"改参数也许有解"式拒绝的地方）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UuidRequest {
    /// 写入调用方给定的值
    Explicit(String),
    /// 要求目标生成一个新的随机值
    NewRandom,
}

/// 该 FS 能接受什么样的 UUID 设置请求（四态，不折成布尔）：
/// - `ExplicitAndGenerated`：既能设指定值，也能生成新值（ext: tune2fs -U uuid|random；
///   xfs: xfs_admin -U uuid|generate；btrfs: btrfstune -f -U uuid / -u）
/// - `ExplicitOnly`：只有指定值形式（swaplabel -U UUID，没有 generate 形式）
/// - `GeneratedOnly`：只支持"生成新值"。ntfs 唯一可改的标识是 `ntfslabel --new-serial`
///   生成的 serial，而它**不是** Windows volume UUID（man ntfslabel）——用户给的具体值
///   无从落实，静默丢弃正是要防的漂移，故必须让调用方显式拒绝
/// - `No`：本工具不为这种类型接 UUID 工具
pub enum UuidSupport {
    ExplicitAndGenerated,
    ExplicitOnly,
    GeneratedOnly,
    No(&'static str),
}

pub fn uuid_support(fstype: Option<ContentKind>) -> UuidSupport {
    match fstype {
        Some(ContentKind::Filesystem(FsKind::Ext | FsKind::Xfs | FsKind::Btrfs)) => UuidSupport::ExplicitAndGenerated,
        Some(ContentKind::Swap) => UuidSupport::ExplicitOnly,
        Some(ContentKind::Filesystem(FsKind::Ntfs)) => UuidSupport::GeneratedOnly,
        _ => UuidSupport::No("not wired to a tool — use the filesystem's own utility"),
    }
}

/// 设置 FS UUID。来源：tune2fs -U uuid|random（man tune2fs）、
/// xfs_admin -U uuid|generate（man xfs_admin）、ntfslabel --new-serial 无值=随机 serial
/// （man ntfslabel）、btrfstune -f -U uuid / -u（-f：change fsid 属 dangerous changes，
/// man btrfstune）、swaplabel -U uuid（man swaplabel，无生成形式）。
/// 请求形式与 FS 能力的匹配由调用方先按 [`uuid_support`] 判定；此处只做工具映射，
/// 落不到工具的组合同样拒绝，不静默降级。
///
/// 工具退 0 不等于写了盘：成功后按同一族工具回读一次（ext `tune2fs -l`、
/// xfs `xfs_admin -u`、btrfs `btrfs filesystem show`、swap `swaplabel`），
/// 显式值要求相等、随机值要求与写前不同。ntfs 的 serial 没有回读出口——
/// `ntfsinfo -m` 不打印卷序列号——那条路径不做核对
pub fn set_uuid(
    src: &mut crate::dev::WriteSource,
    part: u32,
    fstype: Option<ContentKind>,
    req: &UuidRequest,
) -> Result<(), FsError> {
    let (device, _) = partition_device(src, part)?;
    let dev = device.path();
    let random = matches!(req, UuidRequest::NewRandom);
    let Some(uuid_op) = UuidSet::for_request(fstype, random) else {
        return Err(FsError::unsupported(format!(
            "no uuid tool wired for {}{}",
            content_name(fstype.unwrap_or(ContentKind::Unknown)),
            if random { " that generates a new value" } else { "" }
        )));
    };
    // 工具缺失时一个字节都不写：存在性确认也在屏障之前
    uuid_op.tool().require_named("set the uuid")?;
    // 写前基线只服务"生成新值"：没有它就无法区分"换了"与"工具退 0 但没写"。
    // 这一次目标读取同样在屏障之前——读不出就该停在"确定未写盘"
    let before = match req {
        UuidRequest::NewRandom => read_uuid(fstype, dev)?,
        UuidRequest::Explicit(_) => None,
    };
    // 屏障紧贴工具启动：设备已建立、工具已确认、基线已读取
    src.mark_non_reversible()
        .map_err(|e| FsError::from(io::Error::other(e.record_failure("the transaction barrier"))))?;
    let uuid = match req {
        UuidRequest::Explicit(u) => u.as_str(),
        UuidRequest::NewRandom => "",
    };
    let out = uuid_op.write(dev, uuid)?;
    if !out.status.success() {
        return Err(FsError::command(uuid_op.tool().name(), &out));
    }
    let after = read_uuid(fstype, dev)?;
    uuid_readback_verdict(req, before.as_deref(), after.as_deref()).map_err(|why| {
        FsError::CommandFailed(format!(
            "{} reported success but the uuid read-back does not confirm it: {why}",
            uuid_op.tool().name()
        ))
    })
}

/// 回读一次 UUID。`Ok(None)` 只表示这种类型没有可用的回读工具（ntfs 的 serial 不在
/// `ntfsinfo -m` 的输出里）；有工具但读不出或解析不出时返回错误，不拿 `None` 冒充
/// "无需核对"
fn read_uuid(fstype: Option<ContentKind>, dev: &str) -> Result<Option<String>, FsError> {
    match UuidRead::for_fstype(fstype) {
        Some(op) => op.read(dev, fstype),
        None => Ok(None),
    }
}

/// 各工具在 `LC_ALL=C` 下的固定输出：tune2fs -l 的 `Filesystem UUID:`、
/// xfs_admin -u 的 `UUID =`、swaplabel 的 `UUID:`、btrfs filesystem show 的 `uuid:`
pub(crate) fn uuid_from_output(fstype: Option<ContentKind>, text: &str) -> Option<String> {
    let raw = match fstype {
        Some(ContentKind::Filesystem(FsKind::Ext)) => labelled_line(text, "Filesystem UUID:")?,
        Some(ContentKind::Filesystem(FsKind::Xfs)) => labelled_line(text, "UUID =")?,
        Some(ContentKind::Swap) => labelled_line(text, "UUID:")?,
        Some(ContentKind::Filesystem(FsKind::Btrfs)) => {
            let at = text.find("uuid:")?;
            text[at + "uuid:".len()..].split_whitespace().next()?.to_string()
        }
        _ => return None,
    };
    let value = raw.trim();
    (!value.is_empty()).then(|| normalize_uuid(value))
}

/// UUID 的比较口径：去掉分隔符与大括号、转小写。写入期望值与工具回读值都经它，
/// 同一卷在连字符、`{}`、大小写上的差异不会被读成"写后值不对"
pub(crate) fn normalize_uuid(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '-' | '{' | '}'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn labelled_line(text: &str, prefix: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(prefix))
        .map(|value| value.trim().to_string())
}

/// 回读判定。`before` 是写前值（显式值不需要），`after` 是写后值；`None` 表示该类型
/// 没有回读工具、无从核对（ntfs）。显式值按 [`normalize_uuid`] 的口径相等，随机值必须
/// 与写前不同——"工具退 0 但没写"恰好会在这两种判定上失败
pub(crate) fn uuid_readback_verdict(
    req: &UuidRequest,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<(), String> {
    let Some(after) = after else { return Ok(()) };
    match req {
        UuidRequest::Explicit(want) => {
            if normalize_uuid(after) == normalize_uuid(want) {
                Ok(())
            } else {
                Err(format!("expected {want}, read back {after}"))
            }
        }
        UuidRequest::NewRandom => {
            if before.is_some_and(|old| normalize_uuid(old) == normalize_uuid(after)) {
                Err(format!("it is still {after}, the value did not change"))
            } else {
                Ok(())
            }
        }
    }
}