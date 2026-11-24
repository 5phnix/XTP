//! Bounded-memory structural probe for RPF9 archives.
//!
//! Scans an archive stream in 2 KiB (`SECTOR_SIZE`) steps without loading the
//! file into memory and without decrypting anything. It counts `9FPR` headers
//! (root and nested), detects truncated headers at EOF, and checks whether the
//! 32-byte TOC area (`0x40..0x60`) repeats across headers.

use std::collections::BTreeSet;
use std::io::{self, Read};

/// RPF9 header magic (`"9FPR"` at the start of a 2 KiB sector).
pub const RPF9_MAGIC: [u8; 4] = *b"9FPR";

/// Alignment step between RPF9 headers (2 KiB).
pub const SECTOR_SIZE: usize = 2048;

/// Offset of the 32-byte TOC sample region inside an RPF9 header.
pub const TOC_OFFSET: usize = 0x40;

/// Length of the TOC sample region (`0x40..0x60`).
pub const TOC_SAMPLE_LEN: usize = 32;

/// Minimum bytes needed in a sector for a complete header + TOC sample.
pub const MIN_HEADER_LEN: usize = TOC_OFFSET + TOC_SAMPLE_LEN;

/// Maximum number of distinct 32-byte TOC samples retained in memory (128 KiB).
const MAX_TRACKED_TOCS: usize = 4096;

/// Summary produced by [`probe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// Total bytes read from the stream.
    pub bytes_scanned: u64,
    /// Whether offset 0 starts with [`RPF9_MAGIC`].
    pub has_root_header: bool,
    /// Total `9FPR` headers seen on 2 KiB boundaries (root + nested + truncated).
    pub headers: u64,
    /// Complete headers (at least [`MIN_HEADER_LEN`] bytes available).
    pub complete_headers: u64,
    /// Headers found at a non-zero 2 KiB boundary.
    pub nested_headers: u64,
    /// Headers cut short before [`MIN_HEADER_LEN`] bytes at EOF.
    pub truncated_headers: u64,
    /// Complete headers whose `0x40..0x60` TOC sample contains at least one non-zero byte.
    pub nonzero_toc_headers: u64,
    /// Number of complete headers whose TOC sample matched a previously seen sample.
    pub repeated_toc_count: u64,
    /// `true` if at least two complete headers shared an identical TOC sample.
    pub toc_areas_repeat: bool,
    /// `true` if at least two complete headers were seen and all had the exact same TOC sample.
    pub all_tocs_identical: bool,
    /// `true` if more than `MAX_TRACKED_TOCS` distinct TOC samples were seen and
    /// later distinct samples were not inserted to keep memory bounded.
    pub toc_tracking_capped: bool,
}

/// Scan `reader` in 2 KiB steps with bounded memory and summarise RPF9 headers.
///
/// Never decrypts any data and holds at most one 2 KiB sector buffer plus a
/// bounded set of 32-byte TOC samples.
pub fn probe<R: Read>(mut reader: R) -> io::Result<ProbeReport> {
    let mut buf = [0u8; SECTOR_SIZE];
    let mut offset: u64 = 0;

    let mut has_root_header = false;
    let mut headers: u64 = 0;
    let mut complete_headers: u64 = 0;
    let mut nested_headers: u64 = 0;
    let mut truncated_headers: u64 = 0;
    let mut nonzero_toc_headers: u64 = 0;
    let mut repeated_toc_count: u64 = 0;

    let mut first_toc: Option<[u8; TOC_SAMPLE_LEN]> = None;
    let mut all_match_first = true;
    let mut seen_tocs: BTreeSet<[u8; TOC_SAMPLE_LEN]> = BTreeSet::new();
    let mut toc_tracking_capped = false;

    loop {
        let n = read_sector(&mut reader, &mut buf)?;
        if n == 0 {
            break;
        }
        let sector = &buf[..n];
        if sector.len() >= RPF9_MAGIC.len() && sector[..4] == RPF9_MAGIC {
            headers += 1;
            if offset == 0 {
                has_root_header = true;
            } else {
                nested_headers += 1;
            }

            if sector.len() < MIN_HEADER_LEN {
                truncated_headers += 1;
            } else {
                complete_headers += 1;
                let mut sample = [0u8; TOC_SAMPLE_LEN];
                sample.copy_from_slice(&sector[TOC_OFFSET..MIN_HEADER_LEN]);
                if sample.iter().any(|&b| b != 0) {
                    nonzero_toc_headers += 1;
                }
                match first_toc {
                    None => first_toc = Some(sample),
                    Some(first) if first != sample => all_match_first = false,
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
        offset += n as u64;
        if n < SECTOR_SIZE {
            break;
        }
    }

    Ok(ProbeReport {
        bytes_scanned: offset,
        has_root_header,
        headers,
        complete_headers,
        nested_headers,
        truncated_headers,
        nonzero_toc_headers,
        repeated_toc_count,
        toc_areas_repeat: repeated_toc_count > 0,
        all_tocs_identical: complete_headers >= 2 && all_match_first,
        toc_tracking_capped,
    })
}

/// Read up to `buf.len()` bytes, looping over short reads until full or EOF.
fn read_sector<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
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
        assert!(!report.toc_areas_repeat);
        assert!(!report.all_tocs_identical);
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
