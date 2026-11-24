//! Opt-in local captures of the DSP's stream format and first commands.

use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use super::StreamDesc;

pub(super) struct Capture {
    dir: PathBuf,
    blocks: Range<u8>,
}

impl Capture {
    /// A non-null IR in the descriptor must hold `ir_floats` samples.
    pub(super) unsafe fn start(desc: &StreamDesc) -> Option<Self> {
        let root = std::env::var_os("XTP_DSP_DUMP").filter(|s| !s.is_empty())?;
        // SAFETY: the caller supplies the IR for the duration of activation.
        match unsafe { Self::create(Path::new(&root), desc) } {
            Ok(capture) => Some(capture),
            Err(error) => {
                tracing::warn!(%error, "could not start DSP capture");
                None
            }
        }
    }

    unsafe fn create(root: &Path, desc: &StreamDesc) -> io::Result<Self> {
        let root = std::path::absolute(root)?;
        xtp_core::ensure_outside_repo(&root).map_err(io::Error::other)?;
        fs::create_dir_all(&root)?;
        let dir = tempfile::Builder::new()
            .prefix("stream-")
            .tempdir_in(fs::canonicalize(root)?)?;
        let (kind, options, frames, channels, ir_floats) = (
            desc.kind,
            desc.options,
            desc.frames,
            desc.channels,
            desc.ir_floats,
        );
        fs::write(
            dir.path().join("params.txt"),
            format!(
                "kind={kind}\noptions={options}\nframes={frames}\nchannels={channels}\nir_floats={ir_floats}\n"
            ),
        )?;
        // SAFETY: the descriptor gives the IR's extent in floats.
        unsafe { write_samples(&dir.path().join("ir.f32le"), desc.ir, ir_floats as usize) }?;
        Ok(Self {
            dir: dir.keep(),
            blocks: 0..4,
        })
    }

    pub(super) fn next_block(&mut self) -> Option<PathBuf> {
        self.blocks
            .next()
            .map(|n| self.dir.join(format!("block-{n}")))
    }
}

/// A non-null buffer must hold one block of `samples` floats.
pub(super) unsafe fn write_block(path: &Path, side: &str, data: *const f32, samples: usize) {
    let path = path.with_extension(format!("{side}.f32le"));
    // SAFETY: the submitter supplies this block's extent.
    if let Err(error) = unsafe { write_samples(&path, data, samples) } {
        tracing::warn!(%error, path = %path.display(), "could not write DSP block");
    }
}

unsafe fn write_samples(path: &Path, data: *const f32, samples: usize) -> io::Result<()> {
    if samples != 0 && data.is_null() {
        return Ok(());
    }
    // Publish only a complete block. The stream directory already exists.
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    if samples != 0 {
        let bytes = samples
            .checked_mul(size_of::<f32>())
            .filter(|&n| n <= isize::MAX as usize)
            .ok_or_else(|| io::Error::other("DSP buffer size exceeds address space"))?;
        // SAFETY: the caller supplies this many readable bytes. The x64 DSP
        // buffers already contain little-endian words, including the packed IR.
        let data = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), bytes) };
        file.write_all(data)?;
    }
    file.persist_noclobber(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::dsp::*;
    use std::ptr;

    fn submit_in_place(capturing: bool) {
        let mut base = vec![0u8; 0x10000];
        let (mut dsp, mut status, mut stream) = (ptr::null_mut(), ptr::null(), ptr::null_mut());
        assert_eq!(
            XDspConnectWithMaximumStreamLimit(
                base.as_mut_ptr().cast(),
                base.len() as u32,
                1,
                1,
                2,
                &mut dsp
            ),
            S_OK
        );
        for id in 1..=2 {
            let ir = [-0.0, id as f32, -0.25];
            let desc = StreamDesc {
                kind: CONVOLUTION,
                options: 2,
                frames: 512,
                channels: 2,
                ir_floats: ir.len() as u32,
                ir: ir.as_ptr(),
            };
            assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut stream), S_OK);
            // SAFETY: activation returned a live stream.
            assert_eq!(unsafe { (*stream).capture.is_some() }, capturing);
            for n in 0..5 {
                let mut block = [n as f32 + 0.5; 1024];
                let command = Command {
                    gains: [1.0, 1.0],
                    input: block.as_ptr(),
                    output: block.as_mut_ptr(),
                    reserved: 0,
                };
                let mut ticket = 0;
                assert_eq!(XDspSubmitCommand(stream, &command, &mut ticket), S_OK);
                assert_eq!(ticket, n + 1);
                assert_eq!(block, [0.0; 1024]);
            }
            assert_eq!(XDspDeactivate(stream), S_OK);
        }
        assert_eq!(XDspDisconnect(dsp), S_OK);
    }

    #[test]
    fn opt_in_stream_capture() {
        const CHILD: &str = "XTP_TEST_DSP_CAPTURE_CHILD";
        if let Ok(mode) = std::env::var(CHILD) {
            submit_in_place(mode == "on");
            return;
        }
        // Each child gets its own environment, so other DSP tests can run in parallel.
        let tmp = tempfile::tempdir().unwrap();
        let run = |destination: Option<&str>, mode: &str| {
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
            cmd.current_dir(tmp.path())
                .env(CHILD, mode)
                .env_remove("XTP_DSP_DUMP")
                .args([
                    "--exact",
                    "modules::dsp::capture::tests::opt_in_stream_capture",
                ]);
            if let Some(path) = destination {
                cmd.env("XTP_DSP_DUMP", path);
            }
            let output = cmd.output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(None, "off");
        run(Some(""), "off");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);

        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        fs::write(repo.join(".xtp-root"), []).unwrap();
        run(Some("repo/dump"), "off");
        assert!(!repo.join("dump").exists());
        fs::write(tmp.path().join("file"), []).unwrap();
        run(Some("file/dump"), "off");

        run(Some("dump"), "on");
        let mut responses = Vec::new();
        for entry in fs::read_dir(tmp.path().join("dump")).unwrap() {
            let dir = entry.unwrap().path();
            assert_eq!(
                fs::read_to_string(dir.join("params.txt")).unwrap(),
                "kind=1\noptions=2\nframes=512\nchannels=2\nir_floats=3\n"
            );
            responses.push(fs::read(dir.join("ir.f32le")).unwrap());
            for n in 0..4 {
                assert_eq!(
                    fs::read(dir.join(format!("block-{n}.input.f32le"))).unwrap(),
                    (n as f32 + 0.5).to_le_bytes().repeat(1024)
                );
                assert_eq!(
                    fs::read(dir.join(format!("block-{n}.output.f32le"))).unwrap(),
                    vec![0; 1024 * 4]
                );
            }
            assert_eq!(fs::read_dir(dir).unwrap().count(), 10);
        }
        let mut expected = [1.0f32, 2.0].map(|v| {
            [-0.0, v, -0.25]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>()
        });
        expected.sort();
        responses.sort();
        assert_eq!(responses, expected);
    }
}
