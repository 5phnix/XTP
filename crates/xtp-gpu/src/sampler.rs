//! Samplers as the host describes them, from the console's sampler
//! description (`CreateSamplerX`), which names the hardware's sampler
//! fields. (Immediate samplers in shader code come decoded with the
//! translation.)

use windows::Win32::Graphics::Direct3D12::*;

// GFX10 sampler bounds are unsigned 4.8. The signed 14-bit bias has the
// same eight fractional bits. AMD PAL truncates both when packing a sampler.
const LOD_SCALE: f32 = 256.0;
const LOD_MAX: f32 = 4095.0 / LOD_SCALE;

fn fixed_lod(value: f32, min: f32, max: f32) -> f32 {
    if value.is_nan() {
        return 0.0;
    }
    (value.clamp(min, max) * LOD_SCALE).trunc() / LOD_SCALE
}

/// The fields both forms share.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Fields {
    /// Point 0, bilinear 1, anisotropic point 2, anisotropic bilinear 3.
    mag: u32,
    min: u32,
    /// None 0, point 1, linear 2.
    mip: u32,
    /// Plain 0, minimum 1, maximum 2.
    mode: u32,
    /// Host address modes.
    address: [u32; 3],
    anisotropy: u32,
    /// Host comparison function.
    compare: u32,
    min_lod: f32,
    max_lod: f32,
    bias: f32,
    border: [f32; 4],
}

impl Fields {
    /// `aniso_point_mip`: the host filters anisotropically with point mips
    /// ([`aniso_point_mip`]).
    fn host(&self, aniso_point_mip: bool) -> D3D12_SAMPLER_DESC {
        let linear = |f: u32| u32::from(f == 1 || f == 3);
        let aniso = self.mag >= 2 || self.min >= 2;
        // The console compares when the instruction says so, with the
        // sampler's function; the host needs a comparison sampler for that.
        // A sampler given a real test (less ... greater-or-equal) is one;
        // never, always and none are plain samplers.
        const LESS: u32 = 2;
        const GREATER_EQUAL: u32 = 7;
        let compares = (LESS..=GREATER_EQUAL).contains(&self.compare);
        let reduction = match self.mode {
            1 => 2,
            2 => 3,
            _ => u32::from(compares),
        };
        let filter = if aniso {
            // Anisotropic with point mips where the host supports it, else
            // linear mips (0x55). Ported from TaynanGT/Vice-Tay commit
            // dbfb5a97 (upstream 94c1ead4, author Flare, GPL-3.0-or-later).
            0x54 | u32::from(self.mip == 2 || !aniso_point_mip)
        } else {
            linear(self.min) << 4 | linear(self.mag) << 2 | u32::from(self.mip == 2)
        };
        let min_lod = fixed_lod(self.min_lod, 0.0, LOD_MAX);
        let max_lod = if self.mip == 0 {
            // Without a mip filter, pin sampling to the minimum level.
            min_lod
        } else {
            fixed_lod(self.max_lod, 0.0, LOD_MAX).max(min_lod)
        };
        let bias = fixed_lod(self.bias, -32.0, 8191.0 / LOD_SCALE);
        D3D12_SAMPLER_DESC {
            Filter: D3D12_FILTER((reduction << 7 | filter) as i32),
            AddressU: D3D12_TEXTURE_ADDRESS_MODE(self.address[0] as i32),
            AddressV: D3D12_TEXTURE_ADDRESS_MODE(self.address[1] as i32),
            AddressW: D3D12_TEXTURE_ADDRESS_MODE(self.address[2] as i32),
            MipLODBias: bias.clamp(D3D12_MIP_LOD_BIAS_MIN, D3D12_MIP_LOD_BIAS_MAX),
            MaxAnisotropy: if aniso {
                self.anisotropy.clamp(1, 16)
            } else {
                1
            },
            ComparisonFunc: D3D12_COMPARISON_FUNC(self.compare.max(1) as i32),
            BorderColor: self.border,
            MinLOD: min_lod,
            MaxLOD: max_lod,
        }
    }
}

/// Whether the host GPU supports anisotropic filtering with point mip
/// selection (`D3D12_OPTIONS19`). Queried once; older runtimes that do not
/// recognize the query fall back to `false`.
// Ported from TaynanGT/Vice-Tay dbfb5a97 (upstream 94c1ead4, Flare, GPL-3.0-or-later).
pub(crate) fn aniso_point_mip(device: &ID3D12Device) -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
        // SAFETY: `CheckFeatureSupport` writes `options` for the given byte size.
        unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS19,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        }
        .is_ok()
            && options.AnisoFilterWithPointMipSupported.as_bool()
    })
}

/// The console's sampler description (27 words): flags, magnification,
/// minification, mip and depth filters, filter mode, three address modes,
/// minimum and maximum LOD, LOD bias, secondary bias, anisotropy, its bias
/// and threshold, comparison, border color, then tuning words.
/// `aniso_point_mip`: as reported by [`aniso_point_mip`].
pub(crate) fn console(words: &[u32; 27], aniso_point_mip: bool) -> D3D12_SAMPLER_DESC {
    let f = |i: usize| f32::from_bits(words[i]);
    Fields {
        mag: words[1],
        min: words[2],
        mip: words[3],
        mode: words[5],
        address: [words[6], words[7], words[8]],
        min_lod: f(9),
        max_lod: f(10),
        bias: f(11),
        anisotropy: words[13],
        compare: words[16],
        border: [f(17), f(18), f(19), f(20)],
    }
    .host(aniso_point_mip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_descriptions_decode() {
        let mut w = [0u32; 27];
        w[1] = 1; // bilinear magnification
        w[2] = 0; // point minification
        w[3] = 2; // linear mips
        w[6..9].copy_from_slice(&[3, 3, 3]);
        w[10] = 1000.0f32.to_bits();
        let d = console(&w, true);
        assert_eq!(d.Filter.0, 0x5, "min point, mag and mip linear");
        assert_eq!(d.AddressW, D3D12_TEXTURE_ADDRESS_MODE_CLAMP);
        assert_eq!(d.MaxLOD, 4095.0 / 256.0);
        // A less-or-equal test makes a comparison sampler; never does not.
        w[16] = 4;
        assert_eq!(console(&w, true).Filter.0, 0x85);
        assert_eq!(
            console(&w, true).ComparisonFunc,
            D3D12_COMPARISON_FUNC_LESS_EQUAL
        );
        w[16] = 1;
        assert_eq!(console(&w, true).Filter.0, 0x5);
    }

    /// Anisotropic heap samplers keep point mips where the host supports
    /// `D3D12_OPTIONS19` (`AnisoFilterWithPointMipSupported`), and fall back to
    /// linear mips otherwise while preserving comparison and min/max reduction.
    #[test]
    fn anisotropic_samplers_keep_point_mips_where_the_host_can() {
        let mut w = [0u32; 27];
        w[1] = 3; // anisotropic bilinear magnification
        w[2] = 3; // and minification
        w[13] = 8;
        for (mip, supported, filter) in [
            (1, true, 0x54),
            (1, false, 0x55),
            (2, true, 0x55),
            (2, false, 0x55),
            (0, true, 0x54),
        ] {
            w[3] = mip;
            let d = console(&w, supported);
            assert_eq!(
                d.Filter.0, filter,
                "mip filter {mip}, point-mip support {supported}"
            );
            assert_eq!(d.MaxAnisotropy, 8);
        }
        w[3] = 1;
        w[16] = 4; // less or equal: a comparison sampler
        assert_eq!(console(&w, true).Filter.0, 0xd4);
        assert_eq!(console(&w, false).Filter.0, 0xd5);
        w[16] = 0;
        w[5] = 2; // maximum reduction
        assert_eq!(console(&w, true).Filter.0, 0x1d4);
        assert_eq!(console(&w, false).Filter.0, 0x1d5);
        w[5] = 1; // minimum reduction
        assert_eq!(console(&w, true).Filter.0, 0x154);
        assert_eq!(console(&w, false).Filter.0, 0x155);
    }

    /// Creates an anisotropic heap sampler on the active host device (when
    /// present) with the host's reported point-mip capability and verifies no
    /// device removal occurs.
    #[test]
    fn the_host_takes_its_anisotropic_samplers() {
        let Some(gpu) =
            crate::testgpu::device("sampler::tests::the_host_takes_its_anisotropic_samplers")
        else {
            return;
        };
        let host = gpu.host();
        let supported = aniso_point_mip(&host.device);
        let mut w = [0u32; 27];
        (w[1], w[2], w[3], w[13]) = (3, 3, 1, 16);
        w[6..9].copy_from_slice(&[3, 3, 3]); // CLAMP
        let desc = console(&w, supported);
        assert_eq!(desc.Filter.0, if supported { 0x54 } else { 0x55 });
        let heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
            NumDescriptors: 1,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            NodeMask: 0,
        };
        // SAFETY: plain 1-entry sampler heap; `GetCPUDescriptorHandleForHeapStart`
        // points to its sole descriptor slot.
        unsafe {
            let heap: ID3D12DescriptorHeap = host.device.CreateDescriptorHeap(&heap_desc).unwrap();
            host.device
                .CreateSampler(&desc, heap.GetCPUDescriptorHandleForHeapStart());
            let reason = host.device.GetDeviceRemovedReason();
            assert!(reason.is_ok(), "point-mip support {supported}: {reason:?}");
        }
        eprintln!("made an anisotropic sampler, filter {:#x}", desc.Filter.0);
    }

    fn sampler_lods(mip: u32, min: f32, max: f32, bias: f32) -> (f32, f32, f32) {
        let mut words = [0; 27];
        words[3] = mip;
        words[9..12].copy_from_slice(&[min.to_bits(), max.to_bits(), bias.to_bits()]);
        let d = console(&words, true);
        (d.MinLOD, d.MaxLOD, d.MipLODBias)
    }

    #[test]
    fn lod_bounds_stay_ordered_with_and_without_mips() {
        assert_eq!(sampler_lods(2, f32::NAN, 5.0, f32::NAN), (0.0, 5.0, 0.0));
        assert_eq!(sampler_lods(1, 3.5, f32::NAN, 0.0), (3.5, 3.5, 0.0));
        assert_eq!(sampler_lods(2, 3.5, 1.0, 0.0), (3.5, 3.5, 0.0));
        assert_eq!(sampler_lods(0, 3.5, 8.0, 0.0), (3.5, 3.5, 0.0));
        assert_eq!(sampler_lods(0, f32::NAN, 8.0, 0.0), (0.0, 0.0, 0.0));
    }

    #[test]
    fn lod_bounds_match_unsigned_fixed_point() {
        let step = 1.0 / 256.0;
        let last = 16.0 - step;
        assert_eq!(sampler_lods(2, 0.0, last, 0.0), (0.0, last, 0.0));
        assert_eq!(sampler_lods(2, -1.0, 16.0, 0.0), (0.0, last, 0.0));
        assert_eq!(sampler_lods(0, f32::INFINITY, 0.0, 0.0), (last, last, 0.0));
        assert_eq!(
            sampler_lods(2, f32::NEG_INFINITY, f32::INFINITY, 0.0),
            (0.0, last, 0.0)
        );
        assert_eq!(
            sampler_lods(2, 1.75 * step, last - 0.25 * step, 0.0),
            (step, last - step, 0.0)
        );
    }

    #[test]
    fn signed_fixed_point_bias_respects_host_limits() {
        let step = 1.0 / 256.0;
        for (input, expected) in [
            (1.75 * step, step),
            (-1.75 * step, -step),
            (-16.0, -16.0),
            (16.0 - 3.0 * step, 16.0 - 3.0 * step),
            (16.0 - 2.0 * step, D3D12_MIP_LOD_BIAS_MAX),
            (-32.0, D3D12_MIP_LOD_BIAS_MIN),
            (32.0 - step, D3D12_MIP_LOD_BIAS_MAX),
            (f32::NEG_INFINITY, D3D12_MIP_LOD_BIAS_MIN),
            (f32::INFINITY, D3D12_MIP_LOD_BIAS_MAX),
        ] {
            assert_eq!(sampler_lods(2, 0.0, 0.0, input).2, expected, "{input}");
        }
    }
}
