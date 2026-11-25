//! The console builder's own geometry formats.
//!
//! The title's BVH builder (`XboxBvhBuilder.dll`: its batched builds and
//! its rehydrations alike) takes DXR's 56-byte triangle descriptions, but
//! gives three of their fields a meaning DXR does not have. A host DXR
//! build decodes them first (`indices.hlsl`, recorded by
//! [`super::indices::record`]):
//!
//! - **Index format `R16_TYPELESS`: meshlet-packed indices.** At the index
//!   address, its low two bits cleared:
//!   - a 12-byte header: the meshlet count in the low 17 bits of its first
//!     word, the length of the triangle table, in words, in its second;
//!   - the meshlets, 16 bytes each: a meshlet's first vertex entry is bit
//!     14 of its first word, as bit 16, over the low 16 bits of its second;
//!   - the triangles, one word each: three corners, 7-bit entries of their
//!     meshlet, in bits 0, 7 and 14, and the meshlet in bits 21 to 31;
//!   - the vertex entries, 16-bit vertex indices.
//!
//!   A corner's vertex is the entry at its meshlet's first entry plus the
//!   corner's. `IndexCount` is three per triangle, as in DXR.
//! - **Vertex format `R32G32_FLOAT`: quantized positions,** 8 bytes a
//!   vertex, after a 32-byte header: an origin (three floats), the axis
//!   order (a word), a scale (three floats) and 4 unused bytes. A vertex's
//!   first word holds `a` in its bits 8 to 31; its low byte and the low 16
//!   bits of its second word hold `b` (bits 0 to 7, then 8 to 23); the top
//!   16 bits of the second word hold `c`. Each is `scale * q + origin`,
//!   with the header's first, second and third component respectively.
//!   The position is (c, b, a) for axis order 0, (b, c, a) for 1, and
//!   (a, b, c) for any other.
//! - **Vertex stride: 15 bits and a flag.** The builder reads the low 15
//!   bits as the stride. Bit 15 swaps the first and third corners of each
//!   meshlet-packed triangle: it turns its winding.
//!
//! Before this was known, the host read packed indices as 16-bit ones:
//! the header and the tables made "indices" such as 0xF800 or 0xFF79, past
//! their vertices, and thousands of builds a session were refused
//! (`docs/analysis/rt-blas-inputs.md`). It built quantized vertices as
//! (x, y, 0) float pairs.

/// The bytes of the header before quantized vertices.
pub(super) const QUANTIZED_HEADER: u64 = 32;
/// The bytes of a quantized vertex.
pub(super) const QUANTIZED_VERTEX: u64 = 8;
/// The stride bits the builder reads.
pub(super) const STRIDE: u64 = 0x7fff;
/// The stride bit that turns meshlet-packed triangles.
pub(super) const FLIP: u64 = 0x8000;

/// The bytes before a packed index buffer's meshlets.
const HEADER: u64 = 12;
/// The bytes of a meshlet.
const MESHLET: u64 = 16;

/// `count` words of title memory from `at`, if all of them are readable.
fn words(at: u64, count: u64) -> Option<impl Iterator<Item = u32>> {
    let end = at.checked_add(count.checked_mul(4)?)?;
    if count != 0 && !super::readable(at, end) {
        return None;
    }
    Some((0..count).map(move |i| {
        // SAFETY: the range [at, end) is committed, readable title memory.
        unsafe { ((at + 4 * i) as *const u32).read_unaligned() }
    }))
}

/// A meshlet-packed triangle's corners in its meshlet, and its meshlet.
fn corners(triangle: u32) -> ([u32; 3], u64) {
    let corner = |i: u32| (triangle >> (7 * i)) & 0x7f;
    ([corner(0), corner(1), corner(2)], u64::from(triangle >> 21))
}

/// A meshlet's first vertex entry, from its first two words.
fn first_entry(m0: u32, m1: u32) -> u64 {
    u64::from(((m0 >> 14) & 1) << 16 | (m1 & 0xffff))
}

/// The bytes a meshlet-packed index buffer at `at` (4-byte aligned) spans
/// for `triangles` triangles, as title memory holds it now: its header, its
/// triangle words, the meshlets they name and the furthest vertex entry
/// they reach. `None` when a part is not readable title memory. The GPU
/// pass reads no byte past it ([`indices`](super::indices)): a version of
/// the buffer that reaches further when the build runs is refused there.
pub(super) fn packed_bytes(at: u64, triangles: u32) -> Option<u64> {
    let mut header = words(at, 2)?;
    let (h0, h1) = (header.next()?, header.next()?);
    let table = HEADER + MESHLET * u64::from(h0 & 0x1ffff);
    let entries = table + 4 * u64::from(h1);
    let triangles = u64::from(triangles);
    let records = || words(at.checked_add(table)?, triangles);
    let last_meshlet = records()?.map(|t| corners(t).1).max()?;
    let meshlets = MESHLET * (last_meshlet + 1);
    // Their first two words each, from the 12th byte.
    let firsts: Vec<u64> = {
        let meshlet_words: Vec<u32> = words(at.checked_add(HEADER)?, meshlets / 4)?.collect();
        let (meshlets, _) = meshlet_words.as_chunks::<4>();
        meshlets.iter().map(|m| first_entry(m[0], m[1])).collect()
    };
    let last_entry = records()?
        .map(|t| {
            let (c, m) = corners(t);
            firsts[m as usize] + u64::from(c[0].max(c[1]).max(c[2]))
        })
        .max()?;
    Some(
        (table + 4 * triangles)
            .max(HEADER + meshlets)
            .max(entries + 2 * (last_entry + 1)),
    )
}

#[cfg(test)]
pub(super) mod pack {
    //! Title-like inputs in the console builder's formats, made by hand.

    /// A meshlet-packed index buffer of `triangles`, each its meshlet and
    /// its three corners, over meshlets that start at `firsts` of
    /// `entries`; `pad` unused triangle words after them. Its header's
    /// first word carries `high` above the meshlet count.
    pub(in super::super) fn indices(
        firsts: &[u32],
        triangles: &[(u32, [u32; 3])],
        pad: u32,
        entries: &[u16],
        high: u32,
    ) -> Vec<u8> {
        let mut words = vec![
            firsts.len() as u32 | high << 17,
            triangles.len() as u32 + pad,
            0x5eed_0000,
        ];
        for &first in firsts {
            // Other bits of the meshlet's words are not its first entry.
            words.extend([
                (first >> 16) << 14 | 0x2a5,
                0xbeef_0000 | (first & 0xffff),
                0x1234_5678,
                0x9abc_def0,
            ]);
        }
        for &(meshlet, [a, b, c]) in triangles {
            words.push(meshlet << 21 | c << 14 | b << 7 | a);
        }
        words.extend(std::iter::repeat_n(0xffff_ffff, pad as usize));
        let mut bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        bytes.extend(entries.iter().flat_map(|e| e.to_le_bytes()));
        bytes
    }

    /// The 32-byte header of quantized vertices.
    pub(in super::super) fn header(origin: [f32; 3], axes: u32, scale: [f32; 3]) -> Vec<u8> {
        let mut words: Vec<u32> = origin.iter().map(|f| f.to_bits()).collect();
        words.push(axes);
        words.extend(scale.iter().map(|f| f.to_bits()));
        words.push(0);
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// A quantized vertex of fixed-point `a` (24 bits), `b` (24) and `c`
    /// (16), and `pad` bytes after it.
    pub(in super::super) fn vertex(a: u32, b: u32, c: u32, pad: usize) -> Vec<u8> {
        assert!(a < 1 << 24 && b < 1 << 24 && c < 1 << 16);
        let lo = a << 8 | (b & 0xff);
        let hi = (b >> 8) | c << 16;
        let mut bytes = [lo.to_le_bytes(), hi.to_le_bytes()].concat();
        bytes.resize(8 + pad, 0xcd);
        bytes
    }

    /// The fixed-point coordinates (a, b, c) whose decoding under axis
    /// order `axes`, `origin` and `scale` is `position`: exact for the
    /// test's choices.
    pub(in super::super) fn quantize(
        position: [f32; 3],
        axes: u32,
        origin: [f32; 3],
        scale: [f32; 3],
    ) -> [u32; 3] {
        let [x, y, z] = position;
        // Which header component each of a, b, c decodes to.
        let abc = match axes {
            0 => [z, y, x],
            1 => [z, x, y],
            _ => [x, y, z],
        };
        std::array::from_fn(|i| {
            let q = (abc[i] - origin[i]) / scale[i];
            assert!(q >= 0. && q.fract() == 0., "{q} is no fixed-point value");
            assert_eq!(q * scale[i] + origin[i], abc[i], "decodes exactly");
            q as u32
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The furthest bytes of a packed index buffer are its last vertex
    /// entry a triangle reaches, past its triangle words and its meshlets;
    /// a buffer that is not readable whole has none.
    #[test]
    fn rt_packed_bytes_reach_the_furthest_vertex_entry() {
        let entries: Vec<u16> = (0..40).collect();
        // Meshlet 1 starts at entry 10; its corner 7 is entry 17.
        let packed = pack::indices(&[0, 10], &[(0, [0, 1, 2]), (1, [7, 3, 0])], 2, &entries, 5);
        // The header, two meshlets, two triangles and two unused words.
        let entries_at = 12 + 2 * 16 + 4 * 4;
        assert_eq!(packed.len(), entries_at + 2 * 40);
        let at = packed.as_ptr() as u64;
        assert_eq!(packed_bytes(at, 2), Some((entries_at + 2 * 18) as u64));
        // One triangle: entry 2.
        assert_eq!(packed_bytes(at, 1), Some((entries_at + 2 * 3) as u64));
        assert_eq!(packed_bytes(0x10, 1), None, "unreadable memory");
    }

    /// A meshlet's first entry takes bit 14 of its first word as bit 16.
    #[test]
    fn rt_packed_meshlet_entries_reach_past_16_bits() {
        assert_eq!(first_entry(1 << 14 | 0x3fff, 0xffff_0002), 0x1_0002);
        assert_eq!(first_entry(!(1 << 14), 7), 7);
        assert_eq!(corners(5 << 21 | 3 << 14 | 2 << 7 | 1), ([1, 2, 3], 5));
    }
}
