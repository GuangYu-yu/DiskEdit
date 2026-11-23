//! btrfs 身份守卫：只对单设备文件系统放行在线操作，并在前后比对 FSID。

use std::path::Path;

use crate::app::execute::ExecError;
use crate::platform::Platform;

/// 一次在线操作前后的 btrfs 身份。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub fsid: [u8; 16],
    pub devices: u32,
}

/// 读取目标设备所属 btrfs 的顶层信息。
pub fn capture(platform: &dyn Platform, device: &Path) -> Result<Identity, ExecError> {
    let show = platform.btrfs_show(device).map_err(ExecError::from)?;
    Ok(Identity {
        fsid: show.fsid,
        devices: show.devices,
    })
}

/// 只有单设备文件系统才有确定的单盘映射；多设备一律拒绝，不进入写路径。
pub fn ensure_mapped(identity: &Identity) -> Result<(), ExecError> {
    if identity.devices != 1 {
        return Err(ExecError::Unsupported(format!(
            "btrfs 跨 {} 个设备，缺少单盘映射",
            identity.devices
        )));
    }
    Ok(())
}

/// 操作前后必须是同一个文件系统。
pub fn verify_unchanged(expected: &Identity, actual: &Identity) -> Result<(), ExecError> {
    if expected.fsid != actual.fsid {
        return Err(ExecError::Postcondition(
            "btrfs FSID 在校验前后不一致".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(devices: u32) -> Identity {
        Identity {
            fsid: [0x5a; 16],
            devices,
        }
    }

    #[test]
    fn single_device_is_mapped() {
        assert!(ensure_mapped(&identity(1)).is_ok());
    }

    #[test]
    fn multi_device_is_refused() {
        match ensure_mapped(&identity(2)) {
            Err(ExecError::Unsupported(_)) => {}
            other => panic!("应拒绝多设备：{other:?}"),
        }
    }

    #[test]
    fn fsid_change_is_a_postcondition_failure() {
        let mut changed = identity(1);
        changed.fsid[0] = 0x00;
        assert!(verify_unchanged(&identity(1), &changed).is_err());
        assert!(verify_unchanged(&identity(1), &identity(1)).is_ok());
    }
}
