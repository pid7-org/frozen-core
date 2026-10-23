use super::common::{self, PairSlot, QuadSlot};
use frozen_core::memmap::MemMap;

#[test]
fn ok_page_boundary_slot_exact_indices() {
    let page_size = common::get_page_size();
    let spp = common::slots_per_page();

    let (_dir, cfg) = common::create_test_cfg(*b"bndi", 2, page_size * 4);
    let mmap = MemMap::<u64>::new(cfg).expect("mmap creation failed");

    let boundary_slots = [spp - 1, spp, (spp * 2) - 1];
    for (idx, &slot) in boundary_slots.iter().enumerate() {
        let val = 0xAAAA_0000 | (idx as u64);
        mmap.write(slot, |v| *v = val).expect("write at boundary failed");
    }

    for (idx, &slot) in boundary_slots.iter().enumerate() {
        let expected = 0xAAAA_0000 | (idx as u64);
        let actual = mmap.read(slot, |v| *v).expect("read at boundary failed");
        assert_eq!(actual, expected);
    }
}

#[test]
fn ok_multi_slot_spanning_page_boundary() {
    let page_size = common::get_page_size();
    let spp = common::slots_per_page();

    let (_dir, cfg) = common::create_test_cfg(*b"spnb", 2, page_size * 4);
    let mmap = MemMap::<u64>::new(cfg).expect("mmap creation failed");

    let straddle_idx = spp - 1;
    let pair = PairSlot { low: 0x1111_2222_3333_4444, high: 0x5555_6666_7777_8888 };

    mmap.write_exact(straddle_idx, |v: &mut PairSlot| *v = pair)
        .expect("straddle write_exact failed");

    let read_pair =
        mmap.read_exact(straddle_idx, |v: &PairSlot| *v).expect("straddle read_exact failed");
    assert_eq!(read_pair, pair);

    let low_slot = mmap.read(spp - 1, |v| *v).expect("read low slot failed");
    let high_slot = mmap.read(spp, |v| *v).expect("read high slot failed");
    assert_eq!(low_slot, pair.low);
    assert_eq!(high_slot, pair.high);

    let quad_idx = spp - 2;
    let quad = QuadSlot { a: 0xAAAA, b: 0xBBBB, c: 0xCCCC, d: 0xDDDD };

    mmap.write_exact(quad_idx, |v: &mut QuadSlot| *v = quad).expect("quad write_exact failed");

    let read_quad = mmap.read_exact(quad_idx, |v: &QuadSlot| *v).expect("quad read_exact failed");
    assert_eq!(read_quad, quad);
}

#[test]
fn ok_newly_committed_page_zero_initialization_and_alignment() {
    let page_size = common::get_page_size();
    let spp = common::slots_per_page();

    let (_dir, cfg) = common::create_test_cfg(*b"zroa", 1, page_size * 3);
    let mmap = MemMap::<u64>::new(cfg).expect("mmap creation failed");

    mmap.write(spp - 1, |v| *v = 0xFFFF).expect("write last slot failed");
    mmap.grow(2).expect("grow failed");
    assert_eq!(mmap.length(), page_size * 3);

    for slot in spp..(spp * 3) {
        let val = mmap.read(slot, |v| *v).expect("read unwritten slot failed");
        assert_eq!(val, 0);
    }

    for slot in spp..(spp * 3) {
        let pattern = 0x5500_0000 | (slot as u64);
        mmap.write(slot, |v| *v = pattern).expect("write slot failed");
    }

    for slot in spp..(spp * 3) {
        let expected = 0x5500_0000 | (slot as u64);
        let actual = mmap.read(slot, |v| *v).expect("read slot failed");
        assert_eq!(actual, expected);
    }
}

#[test]
fn err_strict_bounds_enforcement_at_page_boundary() {
    let page_size = common::get_page_size();
    let spp = common::slots_per_page();

    let (_dir, cfg) = common::create_test_cfg(*b"bnde", 1, page_size * 2);
    let mmap = MemMap::<u64>::new(cfg).expect("mmap creation failed");

    mmap.write(spp - 1, |v| *v = 0x1234).expect("write last slot failed");
    assert!(mmap.read(spp, |v| *v).is_err());
    assert!(mmap.write(spp, |v| *v = 0x5678).is_err());

    let pair = PairSlot { low: 1, high: 2 };
    assert!(mmap.write_exact(spp - 1, |v: &mut PairSlot| *v = pair).is_err());
    assert!(mmap.read_exact(spp - 1, |v: &PairSlot| *v).is_err());

    mmap.grow(1).expect("grow failed");
    assert_eq!(mmap.length(), page_size * 2);

    assert!(mmap.write_exact(spp - 1, |v: &mut PairSlot| *v = pair).is_ok());
    let read_back = mmap.read_exact(spp - 1, |v: &PairSlot| *v).expect("read_exact failed");
    assert_eq!(read_back, pair);

    assert!(mmap.write_exact((spp * 2) - 1, |v: &mut PairSlot| *v = pair).is_err());
}
