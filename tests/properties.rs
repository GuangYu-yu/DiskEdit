//! 属性用例：不变量在大量输入上的抽样检查。
//!
//! 不引入随机数依赖：用固定种子的线性同余序列生成输入，失败时打印种子，
//! 可完全重放。这里只断言与输入无关的恒等式，不复刻实现。

use diskedit_core::geometry::{align, sector::SectorSize, units};
use diskedit_core::region::{Region, RegionSet};

/// 固定种子的线性同余发生器；只用于生成可重放的输入。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

#[test]
fn alignment_never_moves_a_value_out_of_one_step() {
    let mut rng = Rng(0x005e_eda1_1ce5);
    for _ in 0..2_000 {
        let step = 1u64 << rng.below(21);
        let value = rng.next() >> rng.below(40);
        let up = match align::up(value, step) {
            Ok(up) => up,
            Err(_) => continue,
        };
        let down = align::down(value, step).expect("同一个对齐值应同样合法");
        assert!(down <= value, "down({value}) = {down} 不得高于输入");
        assert!(up >= value, "up({value}) = {up} 不得低于输入");
        assert!(align::is_aligned(up, step), "up({value}) 未对齐到 {step}");
        assert!(
            align::is_aligned(down, step),
            "down({value}) 未对齐到 {step}"
        );
        assert!(up - down <= step, "上取整跨过多步");
        assert_eq!(align::up(up, step), Ok(up), "上取整不幂等");
        assert_eq!(align::down(down, step), Ok(down), "下取整不幂等");
    }
}

#[test]
fn sector_offsets_are_inverse_where_defined() {
    let mut rng = Rng(0x5123_4567_89ab);
    for bytes in [512u64, 4096, 8192, 65536] {
        let sector = SectorSize::new(bytes).expect("测试扇区应合法");
        for _ in 0..500 {
            let lba = rng.next() >> rng.below(32);
            let offset = match sector.offset_of(lba) {
                Some(offset) => offset,
                None => continue,
            };
            assert_eq!(sector.lba_of(offset), Some(lba));
            assert!(offset.is_multiple_of(bytes));
            let arbitrary = rng.next() >> rng.below(40);
            match sector.lba_of(arbitrary) {
                Some(found) => {
                    assert!(arbitrary.is_multiple_of(bytes));
                    assert_eq!(sector.offset_of(found), Some(arbitrary));
                }
                None => assert!(!arbitrary.is_multiple_of(bytes)),
            }
        }
    }
}

#[test]
fn sector_rounding_brackets_the_input() {
    let mut rng = Rng(0xabcd_ef01_2345);
    for bytes in [512u64, 4096] {
        let sector = SectorSize::new(bytes).expect("测试扇区应合法");
        for _ in 0..1_000 {
            let input = rng.next() >> rng.below(40);
            let floor = units::floor_to_sector(input, sector);
            let ceil = units::ceil_to_sector(input, sector).expect("小输入不应溢出");
            assert!(floor <= input && input <= ceil);
            assert!(floor.is_multiple_of(bytes));
            assert!(ceil.is_multiple_of(bytes));
            assert!(ceil - floor <= bytes);
            assert_eq!(
                units::sectors_ceil(input, sector).map(|n| n * bytes),
                Some(ceil)
            );
        }
    }
}

#[test]
fn region_set_maintains_its_invariants_under_union() {
    let mut rng = Rng(0x1111_2222_3333);
    for _ in 0..300 {
        let mut set = RegionSet::new();
        let mut added = Vec::new();
        for _ in 0..20 {
            let start = rng.below(10_000);
            let end = start + rng.below(500);
            let region = Region::new(start, end).expect("起点不大于终点");
            set = RegionSet::from_regions(set.iter().chain(std::iter::once(region)));
            added.push(region);
        }
        let listed = set.regions();
        for pair in listed.windows(2) {
            assert!(pair[0].end() < pair[1].start(), "合并后必须有序且不相邻");
        }
        for region in &added {
            assert!(set.covers(*region), "合并后丢失了 {region:?}");
        }
        let total: u64 = added.iter().map(|region| region.len()).sum();
        assert!(set.total_len().expect("总长不溢出") <= total);
        for region in listed {
            for offset in [region.start(), region.end().saturating_sub(1)] {
                assert!(set.contains_offset(offset));
            }
            assert!(!set.contains_offset(region.end()));
        }
    }
}

#[test]
fn region_intersection_is_symmetric_and_bounded() {
    let mut rng = Rng(0x9999_8888_7777);
    for _ in 0..2_000 {
        let a_start = rng.below(5_000);
        let b_start = rng.below(5_000);
        let a = Region::sized(a_start, rng.below(400)).expect("区间应合法");
        let b = Region::sized(b_start, rng.below(400)).expect("区间应合法");
        assert_eq!(a.overlaps(b), b.overlaps(a));
        assert_eq!(a.intersection(b), b.intersection(a));
        if let Some(both) = a.intersection(b) {
            assert!(a.contains_region(both));
            assert!(b.contains_region(both));
            assert!(!both.is_empty());
        } else {
            assert!(!a.overlaps(b));
        }
    }
}

#[test]
fn generated_reference_mentions_every_declared_syntax() {
    let files = diskedit::docsgen::files();
    let reference = &files
        .iter()
        .find(|(path, _)| *path == "docs/en/reference.md")
        .expect("参考文档应由生成器产出")
        .1;
    for command in diskedit::app::cli::declare::COMMANDS {
        for variant in command.variants {
            let syntax = format!("diskedit {} {}", command.name, variant.syntax);
            assert!(
                reference.contains(&syntax),
                "参考文档缺少声明过的调用形态 {syntax}"
            );
        }
    }
}

#[test]
fn relocation_spans_cover_the_source_exactly_once() {
    let mut rng = Rng(0x0f0f_1e1e_2d2d);
    for _ in 0..500 {
        let copy = 1 + rng.below(64);
        let shift = i64::try_from(rng.below(17)).expect("小整数") - 8;
        let chunk_bytes = 512 * (1 + rng.below(16));
        let Some(layout) = diskedit_core::table::relocate::layout(copy, 512, shift, chunk_bytes)
        else {
            continue;
        };
        let spans = layout.spans(100);
        assert_eq!(spans.len() as u64, layout.count);
        let mut covered = 0u64;
        let mut firsts = Vec::new();
        for span in &spans {
            assert!(span.len_lba > 0 && span.len_lba <= layout.chunk_lba);
            assert!(span.first_lba >= 100 && span.first_lba < 100 + copy);
            covered += span.len_lba;
            firsts.push(span.first_lba);
            let destination = i128::from(span.first_lba) + i128::from(shift);
            assert!(destination >= 0, "目标位置不得为负");
        }
        assert_eq!(covered, copy, "块布局必须恰好覆盖源区间一次");
        let mut expected: Vec<u64> = (0..layout.count)
            .map(|index| 100 + index * layout.chunk_lba)
            .collect();
        expected.sort_unstable();
        firsts.sort_unstable();
        assert_eq!(firsts, expected);
        if shift != 0 && shift.unsigned_abs() < copy {
            assert!(
                layout.chunk_lba <= shift.unsigned_abs(),
                "自重叠时单块不得超过位移"
            );
        }
    }
}
