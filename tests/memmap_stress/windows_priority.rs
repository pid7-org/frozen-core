use super::common;
use frozen_core::memmap::MemMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessInformation, MEMORY_PRIORITY_INFORMATION, MEMORY_PRIORITY_LOW,
    ProcessMemoryPriority,
};

fn query_active_priority() -> u32 {
    let mut info = MEMORY_PRIORITY_INFORMATION { MemoryPriority: 0 };
    let ok = unsafe {
        GetProcessInformation(
            GetCurrentProcess(),
            ProcessMemoryPriority,
            &mut info as *mut _ as *mut core::ffi::c_void,
            core::mem::size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        )
    };
    assert_ne!(ok, 0, "GetProcessInformation failed");
    info.MemoryPriority
}

#[test]
fn ok_concurrent_memory_priority_hint_hammering() {
    let page_size = common::get_page_size();
    let (_dir, cfg) = common::create_test_cfg(*b"prh1", 1, page_size * 4);
    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));

    let num_hint_threads = 4;
    let num_io_threads = 4;
    let total_threads = num_hint_threads + num_io_threads;
    let barrier = Arc::new(Barrier::new(total_threads + 1));
    let stop = Arc::new(AtomicBool::new(false));

    let mut handles = Vec::with_capacity(total_threads);

    // Hammer priority hint repeatedly
    for _ in 0..num_hint_threads {
        let m = Arc::clone(&mmap);
        let b = Arc::clone(&barrier);
        let s = Arc::clone(&stop);

        handles.push(thread::spawn(move || {
            b.wait();
            while !s.load(Ordering::Acquire) {
                assert!(m.apply_memory_priority_hint().is_ok());
                thread::yield_now();
            }
        }));
    }

    // Concurrent writers / readers
    for t in 0..num_io_threads {
        let m = Arc::clone(&mmap);
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();
            let slot = t;
            for i in 0..0x64 {
                let val = (i as u64).wrapping_mul(0xDEAD_BEEF);
                m.write(slot, |v| *v = val).expect("write failed");
                let read_val = m.read(slot, |v| *v).expect("read failed");
                assert_eq!(read_val, val);
            }
        }));
    }

    barrier.wait();

    // Wait for IO threads to complete
    for h in handles.drain(num_hint_threads..) {
        h.join().expect("io thread join failed");
    }

    // Signal hint threads to stop
    stop.store(true, Ordering::Release);
    for h in handles {
        h.join().expect("hint thread join failed");
    }

    let active = query_active_priority();
    assert_eq!(active, MEMORY_PRIORITY_LOW);
}

#[test]
fn ok_concurrent_lifecycle_with_priority_hint_enabled() {
    let page_size = common::get_page_size();
    let num_threads = 4;
    let barrier = Arc::new(Barrier::new(num_threads));

    let mut handles = Vec::with_capacity(num_threads);
    for t in 0..num_threads {
        let b = Arc::clone(&barrier);

        handles.push(thread::spawn(move || {
            b.wait();
            let tag = [b'w', b'p', b'0' + t as u8, b'1'];
            for _ in 0..0x0A {
                let (dir, mut cfg) = common::create_test_cfg(tag, 1, page_size * 2);
                cfg.memory_priority_hint = true;

                let mmap = MemMap::<u64>::new(cfg.clone()).expect("new failed");
                assert!(mmap.apply_memory_priority_hint().is_ok());
                mmap.write(0, |v| *v = 0xCAFE_BABE).expect("write failed");
                assert_eq!(mmap.read(0, |v| *v).expect("read failed"), 0xCAFE_BABE);

                drop(mmap);

                let opened = MemMap::<u64>::open(cfg).expect("open failed");
                assert_eq!(opened.read(0, |v| *v).expect("read failed"), 0xCAFE_BABE);
                assert!(opened.delete().is_ok());
                let _ = dir;
            }
        }));
    }

    for h in handles {
        h.join().expect("lifecycle thread join failed");
    }

    let active = query_active_priority();
    assert_eq!(active, MEMORY_PRIORITY_LOW);
}

#[test]
fn ok_priority_stability_during_dynamic_growth() {
    let page_size = common::get_page_size();
    let (_dir, mut cfg) = common::create_test_cfg(*b"prgr", 1, page_size * 8);
    cfg.memory_priority_hint = true;

    let mmap = Arc::new(MemMap::<u64>::new(cfg).expect("mmap creation failed"));
    let stop = Arc::new(AtomicBool::new(false));

    // Background thread hammering memory priority hint
    let m_hint = Arc::clone(&mmap);
    let s_hint = Arc::clone(&stop);
    let hint_handle = thread::spawn(move || {
        while !s_hint.load(Ordering::Acquire) {
            assert!(m_hint.apply_memory_priority_hint().is_ok());
            thread::yield_now();
        }
    });

    // Main thread dynamically growing mapping
    for _ in 0..4 {
        mmap.grow(1).expect("grow failed");
        thread::sleep(Duration::from_millis(0x05));
    }

    stop.store(true, Ordering::Release);
    hint_handle.join().expect("hint thread join failed");

    let active = query_active_priority();
    assert_eq!(active, MEMORY_PRIORITY_LOW);
}
