# Changelog

## `0.0.33`

- Impl of `isa` module
- Updated docs for `hints` module
- Improved `FrozenError` in `error` module
  - Added `Display`, `std::error::Error` impls
  - Fixed `Debug` to include `context`
  - Shrunk `context` from `String` to `Box<str>` (32 -> 24 bytes)
  - Dropped redundant `is_equal`
- Yanked `ack` module
- Yanked `bufpool` module
- Yanked `crc32` module
- Yanked `ffile` module
- Yanked `fmmap` module
- Yanked `mpscq` module
- Yanked `reservoir` module
- Yanked `wpipe` module

## `0.0.32`

- Fixed the permanent deadlock in `Reservoir::acquire()` (#95)

## `0.0.31`

- Improvements in `reservoir` module
  - Improved docs and doctests
  - Improved internal structure
  - Impl of `Reservoir::retire()` & `Reservoir::insert()` (#87)
  - Updated unit test module expanding test coverage (#89)

## `0.0.30`

- Updated api signature of `FrozenMMap::read()` (#75)
- Fixed dishonore of `immediate_durability` in `FrozenMMapCfg` (#76)
- Impl of `AckTicket::wait()` for internal blocking (#77)
- Remove the stale `durable_cv` from `FrozenMMap -> Core` (#78)
- Fixed `FrozenMMap::delete()` to avoid incorrectly updating `dirty` flag (#79)

## `0.0.29`

- Impl of `reservoir` module (#67)

## `0.0.28`

- Added build targets metadata for `docs.rs` build (#65)

## `0.0.27`

- Implementation of immediate durability mode for the rare write workloads (#63)

## `0.0.26`

- Migrated `wpipe::WritePipe` to use `ack` module for future impl and durability guarantee (#61)

## `0.0.25`

- Migrated `fmmap::FrozenMMap` to use `ack` for future impl and durability guarantee (#57)

## `0.0.24`

- Impl of `ack` module (#58)

## `0.0.23`

- Improved `FrozenError` in `error` module
- Fixed syntax error for bench table in module docs for `bufpool`
- `ffile`
  - Renamed `FFCfg` -> `FrozenFileCfg`
  - Added `module_id` in `FrozenFileCfg`
  - Migrated to use latest `FrozenError`
- `fmmap`
  - Renamed `FMCfg` -> `FrozenMMapCfg`
  - Added `module_id` in `FrozenMMapCfg`
  - Migrated to use latest `FrozenError`
  - Impl of error recovery for lock poisoning on Mutex & RwLock

## `0.0.22`

- Impl of `wpipe` module (#53)

## `0.0.21`

- Fixed following issues
  - #33
  - #34
  - #37
  - #38
  - #41
  - #42
  - #43
  - #44
  - #46
  - #48
  - #50

## `0.0.20`

- Implementation of `bufpool` module

## `0.0.19`

- `FrozenFile`
  - Fixed handling of `_SC_IOV_MAX` in POSIX impl of FrozenFile
  - Increased max number of iovecs allowed on stack (1Kib -> 2Kib)
  - Improved docs
  - Improved test coverage

## `0.0.18`

- Added benches for `crc32` module
- Added benches for `bpool` module
- Added benches for `mpscq` module
- Impl of `FMTransaction` for transactional writes in `FrozenMMap`
- Impl of `FPTransaction` for transactional writes in `FrozenPipe`

## `0.0.17`

- `FrozenMMap`
  - Impl of `FrozenMMap::memory_usage()`
  - Improved internal locking (to fix random SIGSEGV errors)
  - Improved `FrozenMMap::read()` throughput (yanked io_locking)

## `0.0.16`

- `FrozenMMap`
  - Improved Docs
  - Yanked `FrozenMMap::grow`
  - Impl of `FrozenMMap::new_grown`
  - Yanked internal locking for `FrozenMMap`

## `0.0.15`

- Improved notes (internal docs)
- Improved `FrozenMMap::new` api
- Improved `FrozenPipe::new` api
- Improved `MODULE_ID` handling

## `0.0.14`

- Optimized vectored IO for `FrozenFile` on `POSIX` systems

## `0.0.13`

- Improved error handling for `ffile`, `fmmap`, `bpool` & `fpipe` modules
  - **BREAKING:** All error types are private
- No features are available by default

## `0.0.12`

- Impl of `write_sync` in `FrozenMMap`
- Improved epoch handling for `FrozenPipe`

## `0.0.11`

- Impl of `f_advise_raw` (linux only best effort syscall) for `FrozenFile`
- Use of _io_lock_ in `FrozenPipe`

## `0.0.10`

- Impl of `mpscq` module
- Impl of dynamic allocations in `BPool` (via `BPool::allocate_dynamic`)
- Yanked use of lifetimes from `bpool::Allocation` object, w/ use of raw pointer internally
- Impl of `fpipe` module for batch io ops

## `0.0.9`

- Impl of `bpool` module 

## `0.0.8`

- Impl of `crc` module

## `0.0.7`

- `FF`
  - improved public api
  - revised test suites
  - impl of `sync_range` (linux only)
  - impl of vectored write/read ops
  - impl of exclusive locking to prevent simultaneously running multiple instances
  - updated example
  - impl of `FFCfg`
- `FM`
  - improved publinc api
  - impl of `grow` and `delete`
  - completely thread safe, parallel ops on same index w/ internal locking
  - impl of `wait_for_durability`
  - added durability guarantees for write ops
  - simplified locking mechanism

## `0.0.6`

- `FF` improved public api
- `FM` module
  - added `epoch` ids for every writes
  - added `wait_for_durability` to enable waiting for durability
- improved test coverage (80+ tests)
- improved docs

## `0.0.5`

- improved error propogation
- all modules are behind feature flags (all available by default)
- failed attempt to go `no_std`

## `0.0.4`

- `FF` module
  - Yanked `read` & `write` ops
  - Migrated io ops to use `iovecs` for linux impl
  - (Rename) `FF` -> `FrozenFile`
  - Added `mac` support
- `FM` module
  - (Rename) `FM` -> `FrozenMMap`
  - Added `mac` support

## `0.0.3`

- Improved `docs`
- Added `examples/`

## `0.0.2`

- Improved `FFCfg` & `FMCfg` w/ builder pattern construction

## `0.0.1`

- Impl of `fe` (Frozen Error) for custom error repr
- Impl of `hints` module for compiler/branching hints
- Impl of `ff` (FrozenFile) w/ `Linux` (x86 & aarch64)
- Impl of `fm` (FrozenMMAp) w/ `Linux` (x86 & aarch64)
