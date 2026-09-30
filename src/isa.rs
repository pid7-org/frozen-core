//!

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64;

///
#[repr(u8)]
#[cfg(target_arch = "x86_64")]
pub enum ISAExtension {
    ///
    SSE2,

    ///
    SSSE3,

    ///
    SSE4_2,

    ///
    AVX2,

    ///
    AVX512BW,
}

///
#[cfg(target_arch = "aarch64")]
#[repr(u8)]
pub enum ISAExtension {
    ///
    NEON,
}

///
#[inline(always)]
pub fn detect_best_isa_extension() -> ISAExtension {
    #[cfg(target_arch = "aarch64")]
    return ISAExtension::NEON;

    #[cfg(target_arch = "x86_64")]
    return {
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

        ISAExtension::SSE2
    };
}
