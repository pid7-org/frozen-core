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

/// Detects the highest available vector extension supported on `x86_64` systems
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

/// Detects the highest available vector extension supported on `aarch64` systems
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
