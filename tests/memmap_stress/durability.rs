use super::common;
use frozen_core::memmap::MemMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

#[test]
fn ok_epoch_monotonicity_under_concurrent_writers() {
    let page_size = common::get_page_size();
    let (_dir, cfg) = common::create_test_cfg(*b"epcm", 1, page_size * 2);
    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));

    let num_threads = 8;
    let writes_per_thread = 0x40;
    let barrier = Arc::new(Barrier::new(num_threads));
    let epochs = Arc::new(Mutex::new(Vec::with_capacity(num_threads * writes_per_thread)));

    let mut handles = Vec::with_capacity(num_threads);
    for t in 0..num_threads {
        let m = Arc::clone(&mmap);
        let b = Arc::clone(&barrier);
        let ep = Arc::clone(&epochs);

        handles.push(thread::spawn(move || {
            b.wait();
            let mut local_epochs = Vec::with_capacity(writes_per_thread);

            for i in 0..writes_per_thread {
                let slot = (t * writes_per_thread) + i;
                let (ticket, _) = m.write(slot, |v| *v = i as u64).expect("write failed");

                local_epochs.push(ticket.epoch());
            }

            ep.lock().expect("lock failed").extend(local_epochs);
        }));
    }

    for h in handles {
        h.join().expect("thread join failed");
    }

    let mut all_epochs = epochs.lock().expect("lock failed").clone();
    all_epochs.sort_unstable();

    let total_writes = (num_threads * writes_per_thread) as u64;
    assert_eq!(all_epochs.len(), total_writes as usize);

    for (idx, &epoch) in all_epochs.iter().enumerate() {
        assert_eq!(epoch, (idx as u64) + 1);
    }

    assert_eq!(mmap.current_epoch(), total_writes);
}

#[test]
fn ok_durable_epoch_progress_with_concurrent_sync() {
    let page_size = common::get_page_size();
    let (_dir, cfg) = common::create_test_cfg(*b"dprg", 1, page_size * 2);
    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));

    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(6));

    let mut handles = Vec::new();
    for t in 0..4 {
        let m = Arc::clone(&mmap);
        let s = Arc::clone(&stop);
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();
            let mut counter: u64 = 0;

            while !s.load(Ordering::Relaxed) {
                let slot = t * 0x10 + ((counter as usize) % 0x10);
                let _ = m.write(slot, |v| *v = counter);
                counter = counter.wrapping_add(1);
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
                let _ = m.sync();
                let cur = m.current_epoch();
                let dur = m.durable_epoch();

                assert!(dur <= cur);
                thread::yield_now();
            }
        }));
    }

    thread::sleep(Duration::from_millis(0x40));
    stop.store(true, Ordering::Relaxed);

    for h in handles {
        h.join().expect("worker join failed");
    }

    mmap.sync().expect("final sync failed");
    assert_eq!(mmap.durable_epoch(), mmap.current_epoch());
}

#[test]
fn ok_ack_ticket_wait_and_force_stress() {
    let page_size = common::get_page_size();
    let (_dir, cfg) =
        common::create_sync_test_cfg(*b"tktw", 1, page_size * 2, Duration::from_millis(0x10));

    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));

    let num_threads = 4;
    let writes_per_thread = 0x20;
    let barrier = Arc::new(Barrier::new(num_threads));

    let mut handles = Vec::with_capacity(num_threads);
    for t in 0..num_threads {
        let m = Arc::clone(&mmap);
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();

            for i in 0..writes_per_thread {
                let slot = (t * writes_per_thread) + i;
                let (ticket, _) = m.write(slot, |v| *v = i as u64).expect("write failed");

                if i % 2 == 0 {
                    ticket.wait().expect("wait failed");
                } else {
                    ticket.force().expect("force failed");
                }

                assert!(ticket.is_durable());
            }
        }));
    }

    for h in handles {
        h.join().expect("thread join failed");
    }

    assert!(mmap.durable_epoch() >= (num_threads * writes_per_thread) as u64);
}

#[test]
fn ok_durability_across_process_reopen_under_heavy_writes() {
    let page_size = common::get_page_size();
    let spp = common::slots_per_page();

    let (_dir, cfg) = common::create_test_cfg(*b"reop", 2, page_size * 4);
    let total_slots = spp * 2;

    {
        let mmap = MemMap::<u64>::new(cfg.clone()).expect("mmap creation failed");

        for slot in 0..total_slots {
            let val = ((slot as u64) << 0x20) | 0xDEAD_C0DE;
            mmap.write(slot, |v| *v = val).expect("write failed");
        }

        mmap.sync().expect("sync failed");
        assert_eq!(mmap.durable_epoch(), total_slots as u64);
    }

    let reopened = MemMap::<u64>::open(cfg).expect("reopen failed");
    assert_eq!(reopened.length(), page_size * 2);

    for slot in 0..total_slots {
        let expected = ((slot as u64) << 0x20) | 0xDEAD_C0DE;
        let actual = reopened.read(slot, |v| *v).expect("read failed");

        assert_eq!(actual, expected);
    }
}

#[test]
fn ok_tx_commit_advances_epoch_by_one() {
    let page_size = common::get_page_size();
    let (_dir, cfg) = common::create_test_cfg(*b"txep", 1, page_size * 2);
    let mmap = MemMap::<u64>::new(cfg).expect("mmap creation failed");

    let e0 = mmap.current_epoch();
    assert_eq!(e0, 0);

    let mut tx = mmap.new_tx();
    tx.write(&0, &0x1111, 0).expect("stage write 0 failed");
    tx.write(&0, &0x2222, 1).expect("stage write 1 failed");
    tx.write(&0, &0x3333, 2).expect("stage write 2 failed");

    let ticket = tx.commit().expect("tx commit failed");
    assert_eq!(ticket.epoch(), 1);
    assert_eq!(mmap.current_epoch(), 1);

    mmap.sync().expect("sync failed");
    assert_eq!(mmap.durable_epoch(), 1);
    assert!(ticket.is_durable());
}
