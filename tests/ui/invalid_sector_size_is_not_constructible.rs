// 扇区尺寸只经允许集合校验构造：集合外的整数无法直接成为 `SectorSize`。
use diskedit_core::geometry::SectorSize;

fn main() {
    let _sector: SectorSize = 520u64;
}
