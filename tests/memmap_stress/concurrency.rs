use super::common::{self, PairSlot};
use frozen_core::memmap::MemMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

#[test]
fn ok_high_contention_single_slot_cas() {
    let page_size = common::get_page_size();
    let (_dir, cfg) = common::create_test_cfg(*b"cas1", 1, page_size * 2);
    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));

    let num_threads = 8;
    let increments_per_thread = 0x64;
    let barrier = Arc::new(Barrier::new(num_threads + 1));

    let mut handles = Vec::with_capacity(num_threads);
    for _ in 0..num_threads {
        let m = Arc::clone(&mmap);
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();
            for _ in 0..increments_per_thread {
                loop {
                    let cur = m.read(0, |v| *v).expect("read failed");
                    match m.compare_and_write(&cur, &(cur + 1), 0) {
                        Ok(_) => break,
                        Err(_) => thread::yield_now(),
                    }
                }
            }
        }));
    }

    barrier.wait();
    for h in handles {
        h.join().expect("thread join failed");
    }

    let final_val = mmap.read(0, |v| *v).expect("final read failed");
    assert_eq!(final_val, (num_threads * increments_per_thread) as u64);
}

#[test]
fn ok_striped_multi_slot_concurrent_writers() {
    let page_size = common::get_page_size();
    let (_dir, cfg) = common::create_test_cfg(*b"strp", 1, page_size * 2);
    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));

    let num_threads = 8;
    let slots_per_thread = 0x10;
    let iters = 0x64;
    let barrier = Arc::new(Barrier::new(num_threads));

    let mut handles = Vec::with_capacity(num_threads);
    for t in 0..num_threads {
        let m = Arc::clone(&mmap);
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();
            let base_slot = t * slots_per_thread;

            for i in 0..iters {
                for offset in 0..slots_per_thread {
                    let slot = base_slot + offset;
                    let val = ((t as u64) << 0x20) | (i as u64);

                    m.write(slot, |v| *v = val).expect("write failed");
                }
            }
        }));
    }

    for h in handles {
        h.join().expect("thread join failed");
    }

    for t in 0..num_threads {
        let base_slot = t * slots_per_thread;
        for offset in 0..slots_per_thread {
            let slot = base_slot + offset;
            let expected = ((t as u64) << 0x20) | ((iters - 1) as u64);
            let val = mmap.read(slot, |v| *v).expect("read failed");

            assert_eq!(val, expected);
        }
    }
}

#[test]
fn ok_interleaved_readers_writers_multi_slot() {
    let page_size = common::get_page_size();
    let (_dir, cfg) = common::create_test_cfg(*b"intr", 1, page_size * 2);
    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));

    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(6));
    let num_pairs = 8;

    let mut handles = Vec::new();
    for t in 0..3 {
        let m = Arc::clone(&mmap);
        let s = Arc::clone(&stop);
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();
            let mut iter: u64 = 0;

            while !s.load(Ordering::Relaxed) {
                for p in 0..num_pairs {
                    let slot = p * 2;
                    let payload = ((t as u64) << 0x30) | iter;
                    let pair = PairSlot { low: payload, high: !payload };
                    let _ = m.write_exact(slot, |v: &mut PairSlot| *v = pair);
                }

                iter = iter.wrapping_add(1);
            }
        }));
    }

    for _ in 0..3 {
        let m = Arc::clone(&mmap);
        let s = Arc::clone(&stop);
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();

            while !s.load(Ordering::Relaxed) {
                for p in 0..num_pairs {
                    let slot = p * 2;
                    let res = m.read_exact(slot, |v: &PairSlot| *v);

                    if let Ok(pair) = res {
                        if pair.low != 0 || pair.high != 0 {
                            assert_eq!(pair.high, !pair.low);
                        }
                    }
                }
            }
        }));
    }

    thread::sleep(Duration::from_millis(0x40));
    stop.store(true, Ordering::Relaxed);

    for h in handles {
        h.join().expect("thread join failed");
    }
}

#[test]
fn ok_concurrent_tx_optimistic_commit_and_abort() {
    let page_size = common::get_page_size();
    let (_dir, cfg) = common::create_test_cfg(*b"txcg", 1, page_size * 2);
    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));

    let num_workers = 4;
    let iters = 0x32;
    let commits = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(num_workers));

    let mut handles = Vec::with_capacity(num_workers);
    for t in 0..num_workers {
        let m = Arc::clone(&mmap);
        let b = Arc::clone(&barrier);
        let c = Arc::clone(&commits);

        handles.push(thread::spawn(move || {
            b.wait();

            for _ in 0..iters {
                let s0 = m.read(0, |v| *v).expect("read failed");
                let s1 = m.read(1, |v| *v).expect("read failed");

                if s0 != s1 {
                    continue;
                }

                let next = (((t as u64) + 1) << 0x20) | ((s0 & 0xFFFF_FFFF) + 1);
                let mut tx = m.new_tx();

                if tx.write(&s0, &next, 0).is_ok()
                    && tx.write(&s1, &next, 1).is_ok()
                    && tx.commit().is_ok()
                {
                    c.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }

    for h in handles {
        h.join().expect("thread join failed");
    }

    let final0 = mmap.read(0, |v| *v).expect("read failed");
    let final1 = mmap.read(1, |v| *v).expect("read failed");
    assert_eq!(final0, final1);
}

#[test]
fn ok_read_write_grow_interleaving() {
    let page_size = common::get_page_size();
    let spp = common::slots_per_page();

    let (_dir, cfg) = common::create_test_cfg(*b"rwgi", 1, page_size * 4);
    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));

    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(5));

    let mut handles = Vec::new();
    for t in 0..2 {
        let m = Arc::clone(&mmap);
        let s = Arc::clone(&stop);
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();
            let mut val = (t as u64) + 1;

            while !s.load(Ordering::Relaxed) {
                let slot = (val as usize) % (spp / 2);
                let _ = m.write(slot, |v| *v = val);
                val = val.wrapping_add(2);
            }
        }));
    }

    for _ in 0..2 {
        let m = Arc::clone(&mmap);
        let s = Arc::clone(&stop);
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();

            while !s.load(Ordering::Relaxed) {
                let slot = 0;
                let _ = m.read(slot, |v| *v);
            }
        }));
    }

    let m_grow = Arc::clone(&mmap);
    let b_grow = Arc::clone(&barrier);

    let grow_handle = thread::spawn(move || {
        b_grow.wait();

        thread::sleep(Duration::from_millis(0x0A));
        m_grow.grow(1).expect("first grow failed");

        thread::sleep(Duration::from_millis(0x0A));
        m_grow.grow(1).expect("second grow failed");
    });

    grow_handle.join().expect("grow thread failed");
    stop.store(true, Ordering::Relaxed);

    for h in handles {
        h.join().expect("worker join failed");
    }

    assert_eq!(mmap.length(), page_size * 3);
    let new_slot = spp * 2 + 1;

    mmap.write(new_slot, |v| *v = 0xCAFE_BABE).expect("write into grown page failed");
    assert_eq!(mmap.read(new_slot, |v| *v).expect("read failed"), 0xCAFE_BABE);
}
