//! Bounded-memory structural probe for RPF9 archives.
//!
//! Scans an archive stream in 2 KiB (`SECTOR_SIZE`) steps using a fixed 64 KiB
//! read buffer (`CHUNK_SIZE`), without loading the file into memory and without
//! decrypting anything. It counts `9FPR` headers (root and nested), detects
//! truncated headers at EOF, records a capped list of header offsets, and
//! checks whether non-zero 32-byte TOC areas (`0x40..0x60`) repeat across
//! headers.

use std::collections::BTreeSet;
use std::io::{self, Read};

/// RPF9 header magic (`"9FPR"` at the start of a 2 KiB sector).
pub const RPF9_MAGIC: [u8; 4] = *b"9FPR";

/// Alignment step between RPF9 headers (2 KiB).
pub const SECTOR_SIZE: usize = 2048;

/// Fixed read buffer size (64 KiB = 32 sectors) so scanning large files does
/// not issue one syscall per 2 KiB while keeping memory strictly bounded.
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Offset of the 32-byte TOC sample region inside an RPF9 header.
pub const TOC_OFFSET: usize = 0x40;

/// Length of the TOC sample region (`0x40..0x60`).
pub const TOC_SAMPLE_LEN: usize = 32;

/// Minimum bytes needed in a sector for a complete header + TOC sample.
pub const MIN_HEADER_LEN: usize = TOC_OFFSET + TOC_SAMPLE_LEN;

/// Maximum number of distinct non-zero 32-byte TOC samples retained in memory
/// (about 128 KiB of sample bytes, plus `BTreeSet` node overhead).
const MAX_TRACKED_TOCS: usize = 4096;

/// Maximum number of header byte offsets recorded in [`ProbeReport::header_offsets`].
const MAX_RECORDED_OFFSETS: usize = 256;

/// Summary produced by [`probe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// Total bytes read from the stream.
    pub bytes_scanned: u64,
    /// Whether offset 0 starts with [`RPF9_MAGIC`].
    pub has_root_header: bool,
    /// Total `9FPR` headers seen on 2 KiB boundaries (root + nested; truncated
    /// ones are included in those).
    pub headers: u64,
    /// Complete headers (at least [`MIN_HEADER_LEN`] bytes available).
    pub complete_headers: u64,
    /// Headers found at a non-zero 2 KiB boundary.
    pub nested_headers: u64,
    /// Headers cut short before [`MIN_HEADER_LEN`] bytes at EOF.
    pub truncated_headers: u64,
    /// Complete headers whose `0x40..0x60` TOC sample contains at least one non-zero byte.
    pub nonzero_toc_headers: u64,
    /// Number of complete headers whose non-zero TOC sample matched a previously seen sample.
    pub repeated_toc_count: u64,
    /// `true` if at least two complete headers shared an identical non-zero TOC sample.
    pub toc_areas_repeat: bool,
    /// `true` if at least two complete headers with non-zero TOCs were seen and
    /// all of them had the exact same TOC sample.
    pub all_tocs_identical: bool,
    /// `true` if more than `MAX_TRACKED_TOCS` distinct non-zero TOC samples were
    /// seen and later distinct samples were not inserted to keep memory bounded.
    pub toc_tracking_capped: bool,
    /// Byte offsets of up to `MAX_RECORDED_OFFSETS` detected headers, in stream order.
    pub header_offsets: Vec<u64>,
    /// `true` if more than `MAX_RECORDED_OFFSETS` headers were seen.
    pub header_offsets_capped: bool,
}

/// Scan `reader` in 2 KiB steps with bounded memory and summarise RPF9 headers.
///
/// Never decrypts any data and holds at most one [`CHUNK_SIZE`] read buffer
/// plus bounded sets of 32-byte TOC samples and header offsets.
pub fn probe<R: Read>(mut reader: R) -> io::Result<ProbeReport> {
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut chunk_offset: u64 = 0;

    let mut has_root_header = false;
    let mut headers: u64 = 0;
    let mut complete_headers: u64 = 0;
    let mut nested_headers: u64 = 0;
    let mut truncated_headers: u64 = 0;
    let mut nonzero_toc_headers: u64 = 0;
    let mut repeated_toc_count: u64 = 0;

    let mut first_nonzero_toc: Option<[u8; TOC_SAMPLE_LEN]> = None;
    let mut all_nonzero_match_first = true;
    let mut seen_tocs: BTreeSet<[u8; TOC_SAMPLE_LEN]> = BTreeSet::new();
    let mut toc_tracking_capped = false;

    let mut header_offsets = Vec::new();
    let mut header_offsets_capped = false;

    loop {
        let n = read_chunk(&mut reader, &mut buf)?;
        if n == 0 {
            break;
        }
        let mut pos = 0;
        while pos < n {
            let end = (pos + SECTOR_SIZE).min(n);
            let sector = &buf[pos..end];
            let offset = chunk_offset + pos as u64;
            if sector.starts_with(&RPF9_MAGIC) {
                headers += 1;
                if offset == 0 {
                    has_root_header = true;
                } else {
                    nested_headers += 1;
                }
                if header_offsets.len() < MAX_RECORDED_OFFSETS {
                    header_offsets.push(offset);
                } else {
                    header_offsets_capped = true;
                }

                if sector.len() < MIN_HEADER_LEN {
                    truncated_headers += 1;
                } else {
                    complete_headers += 1;
                    let mut sample = [0u8; TOC_SAMPLE_LEN];
                    sample.copy_from_slice(&sector[TOC_OFFSET..MIN_HEADER_LEN]);
                    if sample.iter().any(|&b| b != 0) {
                        nonzero_toc_headers += 1;
                        match first_nonzero_toc {
                            None => first_nonzero_toc = Some(sample),
                            Some(first) if first != sample => all_nonzero_match_first = false,
                            Some(_) => {}
                        }
                        if seen_tocs.contains(&sample) {
                            repeated_toc_count += 1;
                        } else if seen_tocs.len() < MAX_TRACKED_TOCS {
                            seen_tocs.insert(sample);
                        } else {
                            toc_tracking_capped = true;
                        }
                    }
                }
            }
            pos = end;
        }
        chunk_offset += n as u64;
        if n < CHUNK_SIZE {
            break;
        }
    }

    Ok(ProbeReport {
        bytes_scanned: chunk_offset,
        has_root_header,
        headers,
        complete_headers,
        nested_headers,
        truncated_headers,
        nonzero_toc_headers,
        repeated_toc_count,
        toc_areas_repeat: repeated_toc_count > 0,
        all_tocs_identical: nonzero_toc_headers >= 2 && all_nonzero_match_first,
        toc_tracking_capped,
        header_offsets,
        header_offsets_capped,
    })
}

/// Read up to `buf.len()` bytes, looping over short reads until full or EOF.
fn read_chunk<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn make_sector(toc: [u8; TOC_SAMPLE_LEN]) -> [u8; SECTOR_SIZE] {
        let mut s = [0u8; SECTOR_SIZE];
        s[..4].copy_from_slice(&RPF9_MAGIC);
        s[TOC_OFFSET..MIN_HEADER_LEN].copy_from_slice(&toc);
        s
    }

    #[test]
    fn root_header_only() {
        let sector = make_sector([0xab; TOC_SAMPLE_LEN]);
        let report = probe(Cursor::new(sector)).unwrap();
        assert_eq!(report.bytes_scanned, SECTOR_SIZE as u64);
        assert!(report.has_root_header);
        assert_eq!(report.headers, 1);
        assert_eq!(report.complete_headers, 1);
        assert_eq!(report.nested_headers, 0);
        assert_eq!(report.truncated_headers, 0);
        assert_eq!(report.nonzero_toc_headers, 1);
        assert_eq!(report.header_offsets, [0]);
        assert!(!report.toc_areas_repeat);
        assert!(!report.all_tocs_identical);
    }

    #[test]
    fn nested_headers_on_2k_boundaries() {
        let mut data = Vec::new();
        data.extend_from_slice(&make_sector([0x01; TOC_SAMPLE_LEN])); // 0x0000 root
        data.extend_from_slice(&[0u8; SECTOR_SIZE]); // 0x0800 payload
        data.extend_from_slice(&make_sector([0x02; TOC_SAMPLE_LEN])); // 0x1000 nested 1
        data.extend_from_slice(&make_sector([0x03; TOC_SAMPLE_LEN])); // 0x1800 nested 2

        let report = probe(Cursor::new(data)).unwrap();
        assert!(report.has_root_header);
        assert_eq!(report.headers, 3);
        assert_eq!(report.complete_headers, 3);
        assert_eq!(report.nested_headers, 2);
        assert_eq!(report.truncated_headers, 0);
        assert_eq!(report.header_offsets, [0, 0x1000, 0x1800]);
        assert!(!report.toc_areas_repeat);
        assert!(!report.all_tocs_identical);
    }

    #[test]
    fn stream_without_root_header_and_unaligned_magic_ignored() {
        let mut data = vec![0u8; SECTOR_SIZE];
        // Unaligned magic inside sector 0 must be ignored.
        data[128..132].copy_from_slice(&RPF9_MAGIC);
        // Valid nested header at 0x0800.
        data.extend_from_slice(&make_sector([0x33; TOC_SAMPLE_LEN]));

        let report = probe(Cursor::new(data)).unwrap();
        assert!(!report.has_root_header);
        assert_eq!(report.headers, 1);
        assert_eq!(report.nested_headers, 1);
        assert_eq!(report.header_offsets, [0x0800]);
    }

    #[test]
    fn truncated_header_at_eof() {
        let mut data = Vec::new();
        data.extend_from_slice(&make_sector([0x11; TOC_SAMPLE_LEN]));
        // Truncated nested header: magic present, but shorter than MIN_HEADER_LEN.
        let mut short = [0u8; 32];
        short[..4].copy_from_slice(&RPF9_MAGIC);
        data.extend_from_slice(&short);

        let report = probe(Cursor::new(data)).unwrap();
        assert!(report.has_root_header);
        assert_eq!(report.headers, 2);
        assert_eq!(report.complete_headers, 1);
        assert_eq!(report.nested_headers, 1);
        assert_eq!(report.truncated_headers, 1);
    }

    #[test]
    fn identical_toc_areas_are_detected() {
        let toc = [0x5a; TOC_SAMPLE_LEN];
        let mut data = Vec::new();
        data.extend_from_slice(&make_sector(toc));
        data.extend_from_slice(&make_sector(toc));
        data.extend_from_slice(&make_sector(toc));

        let report = probe(Cursor::new(data)).unwrap();
        assert_eq!(report.headers, 3);
        assert_eq!(report.complete_headers, 3);
        assert_eq!(report.repeated_toc_count, 2);
        assert!(report.toc_areas_repeat);
        assert!(report.all_tocs_identical);
    }

    #[test]
    fn zeroed_toc_areas_do_not_count_as_encrypted_repeats() {
        let mut data = Vec::new();
        data.extend_from_slice(&make_sector([0u8; TOC_SAMPLE_LEN]));
        data.extend_from_slice(&make_sector([0u8; TOC_SAMPLE_LEN]));

        let report = probe(Cursor::new(data)).unwrap();
        assert_eq!(report.headers, 2);
        assert_eq!(report.complete_headers, 2);
        assert_eq!(report.nonzero_toc_headers, 0);
        assert_eq!(report.repeated_toc_count, 0);
        assert!(!report.toc_areas_repeat);
        assert!(!report.all_tocs_identical);
    }

    #[test]
    fn different_toc_areas_do_not_repeat() {
        let mut data = Vec::new();
        for i in 1u8..=4 {
            data.extend_from_slice(&make_sector([i; TOC_SAMPLE_LEN]));
        }

        let report = probe(Cursor::new(data)).unwrap();
        assert_eq!(report.headers, 4);
        assert_eq!(report.complete_headers, 4);
        assert_eq!(report.repeated_toc_count, 0);
        assert!(!report.toc_areas_repeat);
        assert!(!report.all_tocs_identical);
    }

    #[test]
    fn toc_tracking_and_offsets_cap_with_bounded_reads() {
        struct SyntheticStream {
            total_sectors: u64,
            next_sector: u64,
            sector_pos: usize,
            current: [u8; SECTOR_SIZE],
            max_read_req: usize,
        }

        impl SyntheticStream {
            fn new(total_sectors: u64) -> Self {
                Self {
                    total_sectors,
                    next_sector: 0,
                    sector_pos: SECTOR_SIZE,
                    current: [0u8; SECTOR_SIZE],
                    max_read_req: 0,
                }
            }
        }

        impl Read for SyntheticStream {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.max_read_req = self.max_read_req.max(buf.len());
                if self.sector_pos == SECTOR_SIZE {
                    if self.next_sector >= self.total_sectors {
                        return Ok(0);
                    }
                    let mut toc = [0u8; TOC_SAMPLE_LEN];
                    let id = self.next_sector + 1;
                    toc[..8].copy_from_slice(&id.to_le_bytes());
                    self.current = make_sector(toc);
                    self.next_sector += 1;
                    self.sector_pos = 0;
                }
                let avail = SECTOR_SIZE - self.sector_pos;
                let n = avail.min(buf.len());
                buf[..n].copy_from_slice(&self.current[self.sector_pos..self.sector_pos + n]);
                self.sector_pos += n;
                Ok(n)
            }
        }

        let mut stream = SyntheticStream::new(MAX_TRACKED_TOCS as u64 + 1);
        let report = probe(&mut stream).unwrap();
        assert!(
            stream.max_read_req <= CHUNK_SIZE,
            "reader was asked for {} bytes (> CHUNK_SIZE {CHUNK_SIZE})",
            stream.max_read_req
        );
        assert_eq!(report.headers, MAX_TRACKED_TOCS as u64 + 1);
        assert!(report.toc_tracking_capped);
        assert!(report.header_offsets_capped);
        assert_eq!(report.header_offsets.len(), MAX_RECORDED_OFFSETS);
        assert!(!report.toc_areas_repeat);
    }

    #[test]
    fn handles_short_reads() {
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.0.len().min(buf.len()).min(7);
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }

        let mut data = Vec::new();
        data.extend_from_slice(&make_sector([0xaa; TOC_SAMPLE_LEN]));
        data.extend_from_slice(&make_sector([0xaa; TOC_SAMPLE_LEN]));
        let report = probe(Trickle(&data)).unwrap();
        assert_eq!(report.headers, 2);
        assert!(report.toc_areas_repeat);
        assert!(report.all_tocs_identical);
    }
}
