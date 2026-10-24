use frozen_core::memmap::{MemMap, MemMapCfg};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, align(8))]
pub struct PairSlot {
    pub low: u64,
    pub high: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C, align(8))]
pub struct QuadSlot {
    pub a: u64,
    pub b: u64,
    pub c: u64,
    pub d: u64,
}

pub fn create_test_cfg(
    name: [u8; 4],
    initial_pages: usize,
    max_len: usize,
) -> (tempfile::TempDir, MemMapCfg) {
    let dir = tempfile::tempdir().expect("failed to create temp directory");
    let cfg = MemMapCfg {
        module_id: 1,
        path: dir.path().to_path_buf(),
        name,
        version: 1,
        initial_pages,
        max_len,
        sync_interval: None,
        memory_priority_hint: false,
        sequential_access_hint: false,
    };

    (dir, cfg)
}

pub fn create_sync_test_cfg(
    name: [u8; 4],
    initial_pages: usize,
    max_len: usize,
    interval: Duration,
) -> (tempfile::TempDir, MemMapCfg) {
    let dir = tempfile::tempdir().expect("failed to create temp directory");
    let cfg = MemMapCfg {
        module_id: 1,
        path: dir.path().to_path_buf(),
        name,
        version: 1,
        initial_pages,
        max_len,
        sync_interval: Some(interval),
        memory_priority_hint: false,
        sequential_access_hint: false,
    };

    (dir, cfg)
}

pub fn get_page_size() -> usize {
    MemMap::<u64>::page_size().expect("failed to query page size").as_usize()
}

pub fn slots_per_page() -> usize {
    get_page_size() / core::mem::size_of::<u64>()
}
