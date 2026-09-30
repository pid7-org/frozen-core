//! A minimal and `no_std` best available ISA extension detection for x86-64 and AArch64 systems

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64;

/// Available ISA extensions on `x86_64` systems
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg(target_arch = "x86_64")]
pub enum ISAExtension {
    /// SSE2 (baseline on x86_64)
    SSE2,

    /// SSSE3
    SSSE3,

    /// SSE4.2
    SSE4_2,

    /// AVX2
    AVX2,

    /// AVX512BW
    AVX512BW,
}

/// Available ISA extensions on `aarch64` systems
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg(target_arch = "aarch64")]
pub enum ISAExtension {
    /// NEON (baseline on AArch64)
    NEON,
}

/// Detects the highest available vector extension supported on `aarch64` systems.
///
/// Returns [`ISAExtension::NEON`] baseline.
///
/// ## Example
///
/// ```
/// use frozen_core::isa::{detect_best_isa_extension, ISAExtension};
///
/// let isa = detect_best_isa_extension();
/// assert_eq!(isa, ISAExtension::NEON);
/// ```
#[inline(always)]
#[cfg(target_arch = "aarch64")]
pub fn detect_best_isa_extension() -> ISAExtension {
    // NOTE:
    //
    // NEON is part of the mandatory AArch64 baseline ISA.
    //
    // The Armv8-A architecture reference manual and AAPCS64 ABI specify Advanced SIMD (NEON) as a
    // required baseline feature, so every conforming AArch64 CPU is guaranteed to implement it.
    //
    // Therefore, we can safely use NEON as the default backend without runtime feature detection.
    //
    // Ref -> https://github.com/ARM-software/abi-aa/blob/main/aapcs64/aapcs64.rst
    ISAExtension::NEON
}

/// Detects the highest available vector extension supported by both CPU and OS on `x86_64` systems.
///
/// Checks `CPUID` and `XCR0` (via `_xgetbv`) to verify OS context switching support for vector
/// registers before enabling AVX2 or AVX-512BW. Falls back to SSE2 baseline.
///
/// ## Example
///
/// ```
/// use frozen_core::isa::{detect_best_isa_extension, ISAExtension};
///
/// let isa = detect_best_isa_extension();
/// assert!(isa >= ISAExtension::SSE2);
/// ```
#[inline(always)]
#[cfg(target_arch = "x86_64")]
pub fn detect_best_isa_extension() -> ISAExtension {
    let cpuid1 = x86_64::__cpuid(1);

    let has_ssse3 = (cpuid1.ecx & (1 << 9)) != 0;
    let has_sse4_2 = (cpuid1.ecx & (1 << 0x14)) != 0;

    let osxsave = (cpuid1.ecx & (1 << 0x1B)) != 0;
    if osxsave {
        let xcr0 = unsafe { x86_64::_xgetbv(0) };
        let xmm_ymm_enabled = (xcr0 & 0b110) == 0b110;

        if xmm_ymm_enabled {
            let cpuid7 = x86_64::__cpuid_count(7, 0);
            let avx512_enabled = (xcr0 & 0b11100110) == 0b11100110;
            let avx512f_bw = (1 << 0x10) | (1 << 0x1E);

            if avx512_enabled && (cpuid7.ebx & avx512f_bw) == avx512f_bw {
                return ISAExtension::AVX512BW;
            }

            if (cpuid7.ebx & (1 << 5)) != 0 {
                return ISAExtension::AVX2;
            }
        }
    }

    if has_sse4_2 {
        return ISAExtension::SSE4_2;
    }

    if has_ssse3 {
        return ISAExtension::SSSE3;
    }

    // NOTE:
    //
    // SSE2 is part of the mandatory x86_64 baseline ISA.
    //
    // The x86_64 System V ABI specifies SSE2 as a required baseline feature, so every conforming
    // x86_64 CPU is guaranteed to implement it.
    //
    // Therefore, we can safely use SSE2 as the default backend without runtime feature detection.
    //
    // Ref -> https://gitlab.com/x86-psABIs/x86-64-ABI/-/blob/master/x86-64-ABI/low-level-sys-info.tex
    ISAExtension::SSE2
}

#[cfg(test)]
mod tests {
    use super::*;

    mod enum_invariants {
        use super::*;
        use core::mem::{align_of, size_of};

        #[test]
        fn ok_enum_representation_and_layout() {
            assert_eq!(size_of::<ISAExtension>(), 1);
            assert_eq!(align_of::<ISAExtension>(), 1);
        }

        #[test]
        #[cfg(target_arch = "x86_64")]
        fn ok_extension_monotonic_ordering() {
            assert!(ISAExtension::SSE2 < ISAExtension::SSSE3);
            assert!(ISAExtension::SSSE3 < ISAExtension::SSE4_2);
            assert!(ISAExtension::SSE4_2 < ISAExtension::AVX2);
            assert!(ISAExtension::AVX2 < ISAExtension::AVX512BW);
        }

        #[test]
        fn ok_hashing_and_equality() {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};

            fn hash_val<T: Hash>(val: &T) -> u64 {
                let mut hasher = DefaultHasher::new();
                val.hash(&mut hasher);
                hasher.finish()
            }

            let a = detect_best_isa_extension();
            let b = detect_best_isa_extension();
            assert_eq!(a, b);
            assert_eq!(hash_val(&a), hash_val(&b));
        }
    }

    mod detection {
        use super::*;

        #[test]
        fn ok_detection_is_deterministic_and_idempotent() {
            let first = detect_best_isa_extension();
            for _ in 0..100 {
                assert_eq!(detect_best_isa_extension(), first);
            }
        }

        #[test]
        fn ok_detection_across_threads() {
            use std::thread;

            let expected = detect_best_isa_extension();
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    thread::spawn(move || {
                        for _ in 0..50 {
                            assert_eq!(detect_best_isa_extension(), expected);
                        }
                    })
                })
                .collect();

            for handle in handles {
                handle.join().unwrap();
            }
        }

        #[test]
        #[cfg(target_arch = "x86_64")]
        fn ok_meets_x86_64_baseline() {
            let detected = detect_best_isa_extension();
            assert!(detected >= ISAExtension::SSE2);
        }

        #[test]
        #[cfg(target_arch = "aarch64")]
        fn ok_meets_aarch64_baseline() {
            let detected = detect_best_isa_extension();
            assert_eq!(detected, ISAExtension::NEON);
        }

        #[test]
        #[cfg(target_arch = "x86_64")]
        fn ok_cross_validate_with_std_feature_detection() {
            let detected = detect_best_isa_extension();

            assert!(std::is_x86_feature_detected!("sse2"));

            match detected {
                ISAExtension::AVX512BW => {
                    assert!(std::is_x86_feature_detected!("avx512bw"));
                    assert!(std::is_x86_feature_detected!("avx512f"));
                    assert!(std::is_x86_feature_detected!("avx2"));
                    assert!(std::is_x86_feature_detected!("sse4.2"));
                    assert!(std::is_x86_feature_detected!("ssse3"));
                }
                ISAExtension::AVX2 => {
                    assert!(std::is_x86_feature_detected!("avx2"));
                    assert!(std::is_x86_feature_detected!("sse4.2"));
                    assert!(std::is_x86_feature_detected!("ssse3"));
                }
                ISAExtension::SSE4_2 => {
                    assert!(std::is_x86_feature_detected!("sse4.2"));
                    assert!(std::is_x86_feature_detected!("ssse3"));
                }
                ISAExtension::SSSE3 => {
                    assert!(std::is_x86_feature_detected!("ssse3"));
                }
                ISAExtension::SSE2 => {}
            }
        }

        #[test]
        #[cfg(target_arch = "aarch64")]
        fn ok_cross_validate_with_std_feature_detection() {
            assert!(std::arch::is_aarch64_feature_detected!("neon"));
        }
    }

    mod execution_smoke {
        use super::*;

        #[test]
        #[cfg(target_arch = "x86_64")]
        fn ok_execute_detected_isa_instructions() {
            let detected = detect_best_isa_extension();

            #[target_feature(enable = "sse2")]
            unsafe fn exec_sse2() {
                let a = core::arch::x86_64::_mm_set1_epi32(42);
                let b = core::arch::x86_64::_mm_set1_epi32(10);
                let c = core::arch::x86_64::_mm_add_epi32(a, b);
                let mut out = [0i32; 4];
                core::arch::x86_64::_mm_storeu_si128(out.as_mut_ptr() as *mut _, c);
                assert_eq!(out, [52; 4]);
            }

            #[target_feature(enable = "ssse3")]
            unsafe fn exec_ssse3() {
                let val = core::arch::x86_64::_mm_set1_epi32(0x01020304);
                let mask = core::arch::x86_64::_mm_setzero_si128();
                let res = core::arch::x86_64::_mm_shuffle_epi8(val, mask);
                let mut out = [0u8; 16];
                core::arch::x86_64::_mm_storeu_si128(out.as_mut_ptr() as *mut _, res);
                assert_eq!(out[0], 0x04);
            }

            #[target_feature(enable = "sse4.2")]
            unsafe fn exec_sse4_2() {
                let crc = core::arch::x86_64::_mm_crc32_u64(!0, 0x123456789ABCDEF0);
                assert_ne!(crc, 0);
            }

            #[target_feature(enable = "avx2")]
            unsafe fn exec_avx2() {
                let a = core::arch::x86_64::_mm256_set1_epi32(100);
                let b = core::arch::x86_64::_mm256_set1_epi32(200);
                let c = core::arch::x86_64::_mm256_add_epi32(a, b);
                let mut out = [0i32; 8];
                core::arch::x86_64::_mm256_storeu_si256(out.as_mut_ptr() as *mut _, c);
                assert_eq!(out, [300; 8]);
            }

            #[target_feature(enable = "avx512bw,avx512f")]
            unsafe fn exec_avx512bw() {
                let a = core::arch::x86_64::_mm512_set1_epi8(7);
                let b = core::arch::x86_64::_mm512_set1_epi8(3);
                let c = core::arch::x86_64::_mm512_add_epi8(a, b);
                let mut out = [0i8; 64];
                core::arch::x86_64::_mm512_storeu_si512(out.as_mut_ptr() as *mut _, c);
                assert_eq!(out, [10; 64]);
            }

            unsafe {
                if detected >= ISAExtension::SSE2 {
                    exec_sse2();
                }
                if detected >= ISAExtension::SSSE3 {
                    exec_ssse3();
                }
                if detected >= ISAExtension::SSE4_2 {
                    exec_sse4_2();
                }
                if detected >= ISAExtension::AVX2 {
                    exec_avx2();
                }
                if detected >= ISAExtension::AVX512BW {
                    exec_avx512bw();
                }
            }
        }

        #[test]
        #[cfg(target_arch = "aarch64")]
        fn ok_execute_detected_isa_instructions() {
            let detected = detect_best_isa_extension();
            assert_eq!(detected, ISAExtension::NEON);

            #[target_feature(enable = "neon")]
            unsafe fn exec_neon() {
                let v = core::arch::aarch64::vdupq_n_u8(0x42);
                let mut out = [0u8; 16];
                core::arch::aarch64::vst1q_u8(out.as_mut_ptr(), v);
                assert_eq!(out, [0x42; 16]);
            }

            unsafe {
                exec_neon();
            }
        }
    }
}
