//! 对齐：对齐值必须是 2 的幂，向上与向下取整都带溢出检查。

use super::GeometryError;

/// 校验对齐值：非零且为 2 的幂。
pub fn validate(align: u64) -> Result<(), GeometryError> {
    if align.is_power_of_two() {
        Ok(())
    } else {
        Err(GeometryError::MisalignedPowerOfTwo(align))
    }
}

pub fn is_aligned(value: u64, align: u64) -> bool {
    align.is_power_of_two() && value.is_multiple_of(align)
}

/// 向上取整到 `align` 的倍数；`align` 非法或溢出返回错误。
pub fn up(value: u64, align: u64) -> Result<u64, GeometryError> {
    validate(align)?;
    let mask = align - 1;
    let bumped = value.checked_add(mask).ok_or(GeometryError::Overflow)?;
    Ok(bumped & !mask)
}

/// 向下取整到 `align` 的倍数；`align` 非法返回错误。
pub fn down(value: u64, align: u64) -> Result<u64, GeometryError> {
    validate(align)?;
    Ok(value & !(align - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_power_of_two() {
        assert_eq!(validate(0), Err(GeometryError::MisalignedPowerOfTwo(0)));
        assert_eq!(validate(3), Err(GeometryError::MisalignedPowerOfTwo(3)));
        assert_eq!(validate(1), Ok(()));
        assert_eq!(validate(1 << 20), Ok(()));
    }

    #[test]
    fn up_rounds_to_multiple() {
        assert_eq!(up(0, 4096), Ok(0));
        assert_eq!(up(1, 4096), Ok(4096));
        assert_eq!(up(4096, 4096), Ok(4096));
        assert_eq!(up(4097, 4096), Ok(8192));
        assert_eq!(up(1, 1), Ok(1));
    }

    #[test]
    fn down_rounds_to_multiple() {
        assert_eq!(down(0, 4096), Ok(0));
        assert_eq!(down(4095, 4096), Ok(0));
        assert_eq!(down(4096, 4096), Ok(4096));
        assert_eq!(down(8191, 4096), Ok(4096));
    }

    #[test]
    fn extremes_do_not_wrap() {
        assert_eq!(up(u64::MAX, 4096), Err(GeometryError::Overflow));
        assert_eq!(up(u64::MAX - 4095, 4096), Ok(u64::MAX - 4095));
        assert_eq!(down(u64::MAX, 4096), Ok(!4095));
        assert!(is_aligned(u64::MAX - 4095, 4096));
        assert!(!is_aligned(1, 0));
    }

    #[test]
    fn errors_propagate_to_caller() {
        assert_eq!(up(10, 3), Err(GeometryError::MisalignedPowerOfTwo(3)));
        assert_eq!(down(10, 0), Err(GeometryError::MisalignedPowerOfTwo(0)));
    }
}
