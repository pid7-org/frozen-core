use super::common;
use frozen_core::memmap::MemMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

#[test]
fn ok_rapid_sequential_grow_under_load() {
    let page_size = common::get_page_size();
    let spp = common::slots_per_page();
    let target_pages = 8;
    let (_dir, cfg) = common::create_test_cfg(*b"grsq", 1, page_size * target_pages);
    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));

    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(4));

    let mut handles = Vec::new();
    for t in 0..3 {
        let m = Arc::clone(&mmap);
        let s = Arc::clone(&stop);
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();
            let mut counter: u64 = 0;

            while !s.load(Ordering::Relaxed) {
                let slot = (t * 0x10) + ((counter as usize) % 0x10);
                let _ = m.write(slot, |v| *v = counter);
                let _ = m.read(slot, |v| *v);

                counter = counter.wrapping_add(1);
            }
        }));
    }

    barrier.wait();
    for p in 2..=target_pages {
        mmap.grow(1).expect("sequential grow failed");
        assert_eq!(mmap.length(), p * page_size);

        let new_slot = (p - 1) * spp;
        let prev_slot = new_slot - 1;
        assert_eq!(mmap.read(new_slot, |v| *v).expect("read new slot failed"), 0);

        mmap.write(prev_slot, |v| *v = 0xAAAA_BBBB).expect("write prev slot failed");
        assert_eq!(mmap.read(prev_slot, |v| *v).expect("read prev slot failed"), 0xAAAA_BBBB);
    }

    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().expect("worker join failed");
    }

    assert_eq!(mmap.length(), target_pages * page_size);
}

#[test]
fn ok_concurrent_grow_racing_workers() {
    let page_size = common::get_page_size();
    let spp = common::slots_per_page();
    let target_pages = 6;
    let (_dir, cfg) = common::create_test_cfg(*b"grrc", 1, page_size * target_pages);
    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));

    let num_growers = 4;
    let num_readers = 3;
    let barrier = Arc::new(Barrier::new(num_growers + num_readers));
    let stop = Arc::new(AtomicBool::new(false));

    let mut reader_handles = Vec::new();
    let mut grower_handles = Vec::new();

    for _ in 0..num_readers {
        let m = Arc::clone(&mmap);
        let s = Arc::clone(&stop);
        let b = Arc::clone(&barrier);

        reader_handles.push(thread::spawn(move || {
            b.wait();

            while !s.load(Ordering::Relaxed) {
                let _ = m.read(0, |v| *v);
                let _ = m.write(0, |v| *v = v.wrapping_add(1));
            }
        }));
    }

    for _ in 0..num_growers {
        let m = Arc::clone(&mmap);
        let b = Arc::clone(&barrier);

        grower_handles.push(thread::spawn(move || {
            b.wait();
            while m.length() < target_pages * page_size {
                let _ = m.grow(1);
            }
        }));
    }

    for h in grower_handles {
        h.join().expect("grower join failed");
    }

    stop.store(true, Ordering::Relaxed);
    for h in reader_handles {
        h.join().expect("reader join failed");
    }

    assert_eq!(mmap.length(), target_pages * page_size);

    for p in 1..target_pages {
        let slot = p * spp;
        mmap.write(slot, |v| *v = 0x1234_5678).expect("write failed");
        assert_eq!(mmap.read(slot, |v| *v).expect("read failed"), 0x1234_5678);
    }
}

#[test]
fn ok_boundary_slot_hammering_during_growth() {
    let page_size = common::get_page_size();
    let spp = common::slots_per_page();
    let (_dir, cfg) = common::create_test_cfg(*b"bndh", 1, page_size * 4);
    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));

    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(3));

    let mut handles = Vec::new();
    for t in 0..2 {
        let m = Arc::clone(&mmap);
        let s = Arc::clone(&stop);
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();

            let boundary_slot = spp - 1;
            let mut val: u64 = (t as u64) << 0x20;

            while !s.load(Ordering::Relaxed) {
                let _ = m.write(boundary_slot, |v| *v = val);
                let _ = m.read(boundary_slot, |v| *v);
                val = val.wrapping_add(1);
            }
        }));
    }

    barrier.wait();
    for p in 2..=4 {
        thread::sleep(Duration::from_millis(0x0A));
        mmap.grow(1).expect("grow failed");
        assert_eq!(mmap.length(), p * page_size);

        let new_boundary = (p * spp) - 1;
        mmap.write(new_boundary, |v| *v = 0xC0DE).expect("write at new boundary failed");
        assert_eq!(mmap.read(new_boundary, |v| *v).expect("read failed"), 0xC0DE);
    }

    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().expect("worker join failed");
    }
}

#[test]
fn err_grow_beyond_max_len_clean_recovery() {
    let page_size = common::get_page_size();
    let (_dir, cfg) = common::create_test_cfg(*b"grov", 1, page_size * 2);
    let mmap = MemMap::<u64>::new(cfg).expect("mmap creation failed");

    mmap.write(0, |v| *v = 0xCAFE).expect("write failed");
    mmap.grow(1).expect("grow within bounds failed");
    assert_eq!(mmap.length(), page_size * 2);

    let _err = mmap.grow(1).expect_err("grow past max_len must fail");
    assert_eq!(mmap.length(), page_size * 2);

    assert_eq!(mmap.read(0, |v| *v).expect("read failed"), 0xCAFE);
    mmap.write(0, |v| *v = 0xBABE).expect("subsequent write must succeed");
    assert_eq!(mmap.read(0, |v| *v).expect("read failed"), 0xBABE);
}
