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
    fn host(&self) -> D3D12_SAMPLER_DESC {
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
            0x55
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

/// The console's sampler description (27 words): flags, magnification,
/// minification, mip and depth filters, filter mode, three address modes,
/// minimum and maximum LOD, LOD bias, secondary bias, anisotropy, its bias
/// and threshold, comparison, border color, then tuning words.
pub(crate) fn console(words: &[u32; 27]) -> D3D12_SAMPLER_DESC {
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
    .host()
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
        let d = console(&w);
        assert_eq!(d.Filter.0, 0x5, "min point, mag and mip linear");
        assert_eq!(d.AddressW, D3D12_TEXTURE_ADDRESS_MODE_CLAMP);
        assert_eq!(d.MaxLOD, 4095.0 / 256.0);
        // A less-or-equal test makes a comparison sampler; never does not.
        w[16] = 4;
        assert_eq!(console(&w).Filter.0, 0x85);
        assert_eq!(console(&w).ComparisonFunc, D3D12_COMPARISON_FUNC_LESS_EQUAL);
        w[16] = 1;
        assert_eq!(console(&w).Filter.0, 0x5);
    }

    fn sampler_lods(mip: u32, min: f32, max: f32, bias: f32) -> (f32, f32, f32) {
        let mut words = [0; 27];
        words[3] = mip;
        words[9..12].copy_from_slice(&[min.to_bits(), max.to_bits(), bias.to_bits()]);
        let d = console(&words);
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
