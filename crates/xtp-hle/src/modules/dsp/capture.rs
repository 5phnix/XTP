//! Opt-in local captures of the DSP's stream format and first commands.

use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, mpsc};

use super::{Command, StreamDesc};

const MAX_STREAMS: usize = 32;
const MAX_IR_BYTES: usize = 4 * 1024 * 1024;

// At most 32 IRs and four command snapshots per stream can be queued
// in the process. Sending never waits for space or for the filesystem.
struct Writer {
    jobs: mpsc::Sender<Box<dyn FnOnce() + Send>>,
    root: PathBuf,
    streams: AtomicUsize,
}

impl Writer {
    fn open(root: PathBuf) -> io::Result<Self> {
        if !root.is_absolute() {
            return Err(io::Error::other("XTP_DSP_DUMP needs an absolute directory"));
        }
        xtp_core::ensure_outside_repo(&root).map_err(io::Error::other)?;
        fs::create_dir_all(&root)?;
        let root = fs::canonicalize(root)?;
        let (jobs, queue) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        std::thread::Builder::new()
            .name("xtp DSP capture".into())
            .spawn(move || {
                for job in queue {
                    job();
                }
            })?;
        Ok(Self {
            jobs,
            root,
            streams: AtomicUsize::new(0),
        })
    }

    fn submit(&self, job: impl FnOnce() -> io::Result<()> + Send + 'static) {
        let _ = self.jobs.send(Box::new(move || {
            if let Err(error) = job() {
                tracing::warn!(%error, "could not write DSP capture");
            }
        }));
    }
}

pub(super) struct Connection {
    writer: &'static Writer,
    ir_seconds: u32,
    max_channels: u32,
}

impl Connection {
    pub(super) fn open(ir_seconds: u32, max_channels: u32) -> Option<Self> {
        static WRITER: OnceLock<Option<Writer>> = OnceLock::new();
        let writer = WRITER
            .get_or_init(|| {
                let root = std::env::var_os("XTP_DSP_DUMP").filter(|s| !s.is_empty())?;
                Writer::open(root.into())
                    .inspect_err(|error| {
                        tracing::warn!(%error, "could not start DSP capture");
                    })
                    .ok()
            })
            .as_ref()?;
        Some(Self {
            writer,
            ir_seconds,
            max_channels,
        })
    }

    pub(super) unsafe fn activate(&self, desc: &StreamDesc) -> Option<Capture> {
        let writer = self.writer;
        let id = writer
            .streams
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < MAX_STREAMS).then_some(n + 1)
            })
            .ok()?;
        let (kind, options, frames, channels, ir_floats) = (
            desc.kind,
            desc.options,
            desc.frames,
            desc.channels,
            desc.ir_floats,
        );
        let params = format!(
            "kind={kind}\noptions={options}\nframes={frames}\nchannels={channels}\nir_floats={ir_floats}\nir_seconds={}\nmax_channels={}\n",
            self.ir_seconds, self.max_channels
        );
        let oversized = ir_floats as usize > MAX_IR_BYTES / size_of::<f32>();
        let ir = if oversized {
            None
        } else {
            // SAFETY: the descriptor supplies the IR during activation.
            unsafe { copy_samples(desc.ir, ir_floats as usize) }
        };
        let dir = Arc::new(OnceLock::new());
        let destination = Arc::clone(&dir);
        writer.submit(move || {
            if ir.is_none() {
                tracing::warn!(
                    id,
                    ir_floats,
                    oversized,
                    "omitting unavailable DSP impulse response"
                );
            }
            let tmp = tempfile::Builder::new()
                .prefix(&format!("stream-{id:03}-"))
                .tempdir_in(&writer.root)?;
            write_file(tmp.path(), "params.txt", Some(params.as_bytes()))?;
            write_file(tmp.path(), "ir.f32le", ir.as_deref())?;
            let path = tmp.keep();
            tracing::info!(id, path = %path.display(), "captured DSP stream");
            let _ = destination.set(path);
            Ok(())
        });
        Some(Capture {
            writer,
            dir,
            blocks: 0..4,
        })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Flush work queued before disconnect, outside the command completion path.
        let (done, wait) = mpsc::channel();
        self.writer.submit(move || {
            let _ = done.send(());
            Ok(())
        });
        let _ = wait.recv();
    }
}

pub(super) struct Capture {
    writer: &'static Writer,
    dir: Arc<OnceLock<PathBuf>>,
    blocks: Range<u8>,
}

impl Capture {
    pub(super) unsafe fn begin_block(
        &mut self,
        command: &Command,
        samples: usize,
    ) -> Option<Block> {
        Some(Block {
            n: self.blocks.next()?,
            writer: self.writer,
            dir: Arc::clone(&self.dir),
            gains: command.gains,
            // SAFETY: the input holds one block and may alias the output.
            input: unsafe { copy_samples(command.input, samples) },
            #[cfg(windows)]
            // SAFETY: on Windows the kernel copies bytes into initialized storage.
            // The destination might never have been initialized as Rust floats.
            output_before: unsafe { copy_samples(command.output, samples) },
            output: None,
        })
    }
}

pub(super) struct Block {
    n: u8,
    writer: &'static Writer,
    dir: Arc<OnceLock<PathBuf>>,
    gains: [f32; 2],
    input: Option<Vec<u8>>,
    #[cfg(windows)]
    output_before: Option<Vec<u8>>,
    pub(super) output: Option<Vec<u8>>,
}

impl Block {
    pub(super) fn enqueue(self) {
        self.writer.submit(move || {
            let Some(dir) = self.dir.get() else {
                return Ok(());
            };
            let n = self.n;
            let gains = self.gains.map(f32::to_le_bytes).concat();
            for (side, data) in [
                ("input", self.input.as_deref()),
                #[cfg(windows)]
                ("output-before", self.output_before.as_deref()),
                ("output", self.output.as_deref()),
                ("gains", Some(gains.as_slice())),
            ] {
                write_file(dir, &format!("block-{n}.{side}.f32le"), data)?;
            }
            Ok(())
        });
    }
}

fn write_file(dir: &Path, name: &str, data: Option<&[u8]>) -> io::Result<()> {
    if let Some(data) = data {
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(data)?;
        file.persist_noclobber(dir.join(name))?;
    }
    Ok(())
}

/// Caller bounds the count. Non-Windows callers must supply a readable buffer.
pub(super) unsafe fn copy_samples(data: *const f32, samples: usize) -> Option<Vec<u8>> {
    if samples == 0 {
        return Some(Vec::new());
    }
    if data.is_null() {
        return None;
    }
    let bytes = samples.checked_mul(size_of::<f32>())?;
    #[cfg(windows)]
    {
        let mut copy = vec![0; bytes];
        xtp_loader::read_title_memory(data as usize, &mut copy).then_some(copy)
    }
    #[cfg(not(windows))]
    // SAFETY: the caller supplies the stated buffer. XTP targets little-endian x64.
    unsafe {
        Some(std::slice::from_raw_parts(data.cast::<u8>(), bytes).to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::dsp::*;
    use std::ptr;

    fn submit_in_place(capturing: bool) {
        let mut base = vec![0u8; 0x10000];
        let (mut dsp, mut status, mut stream) = (ptr::null_mut(), ptr::null(), ptr::null_mut());
        for id in 1..=if capturing { MAX_STREAMS + 1 } else { 2 } {
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
            let pause = if capturing && id == 1 {
                let (resume, wait) = mpsc::channel();
                // SAFETY: connect returned a live DSP. Hold its writer until submission returns.
                unsafe { (*dsp).capture.as_ref().unwrap() }
                    .writer
                    .submit(move || {
                        let _ = wait.recv_timeout(std::time::Duration::from_secs(10));
                        Ok(())
                    });
                Some(resume)
            } else {
                None
            };
            let mut ir = if id == 4 {
                vec![0.25; MAX_IR_BYTES / size_of::<f32>()]
            } else {
                vec![-0.0, id as f32, -0.25]
            };
            let desc = StreamDesc {
                kind: CONVOLUTION,
                options: 2,
                frames: 512,
                channels: 2,
                ir_floats: if id == 3 { u32::MAX } else { ir.len() as u32 },
                // Windows validates the range through the kernel before copying.
                ir: if cfg!(windows) && id == 5 {
                    ptr::dangling()
                } else {
                    ir.as_ptr()
                },
            };
            assert_eq!(XDspActivate(dsp, &desc, &mut status, &mut stream), S_OK);
            // SAFETY: activation returned a live stream.
            let has_capture = unsafe { (*stream).capture.is_some() };
            assert_eq!(has_capture, capturing && id <= MAX_STREAMS);
            ir.fill(0.0);
            for n in 0..if id <= 2 { 5 } else { 0 } {
                let mut block = [n as f32 + 0.5; 1024];
                let input = [-1.0; 1024];
                let command = Command {
                    gains: [0.25, -0.5],
                    input: if id == 1 {
                        block.as_ptr()
                    } else {
                        input.as_ptr()
                    },
                    output: block.as_mut_ptr(),
                    reserved: 0,
                };
                let mut ticket = 0;
                assert_eq!(XDspSubmitCommand(stream, &command, &mut ticket), S_OK);
                assert_eq!(ticket, n + 1);
                // SAFETY: the live stream owns this status until deactivation.
                assert_eq!(unsafe { (*status).completed }, ticket);
                assert_eq!(block, [0.0; 1024]);
            }
            if let Some(resume) = pause {
                let dump = std::env::var_os("XTP_DSP_DUMP").unwrap();
                assert_eq!(fs::read_dir(dump).unwrap().count(), 0);
                resume.send(()).unwrap();
            }
            assert_eq!(XDspDeactivate(stream), S_OK);
            assert_eq!(XDspDisconnect(dsp), S_OK);
        }
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
        let run = |destination: Option<&Path>, mode: &str| {
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
        run(Some(Path::new("")), "off");
        run(Some(Path::new("relative")), "off");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);

        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        fs::write(repo.join(".xtp-root"), []).unwrap();
        run(Some(&repo.join("dump")), "off");
        assert!(!repo.join("dump").exists());
        fs::write(tmp.path().join("file"), []).unwrap();
        run(Some(&tmp.path().join("file/dump")), "off");

        let dump = tmp.path().join("dump");
        run(Some(&dump), "on");
        assert_eq!(fs::read_dir(&dump).unwrap().count(), MAX_STREAMS);
        let mut dirs = fs::read_dir(dump)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        dirs.sort();
        for (id, dir) in dirs.iter().enumerate() {
            assert!(
                dir.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .starts_with(&format!("stream-{id:03}-"))
            );
            if id == 2 || (cfg!(windows) && id == 4) {
                assert!(dir.join("params.txt").is_file());
                assert!(!dir.join("ir.f32le").exists());
                continue;
            }
            if id >= 3 {
                if id == 3 {
                    assert_eq!(
                        fs::metadata(dir.join("ir.f32le")).unwrap().len(),
                        MAX_IR_BYTES as u64
                    );
                }
                continue;
            }
            assert_eq!(
                fs::read_to_string(dir.join("params.txt")).unwrap(),
                "kind=1\noptions=2\nframes=512\nchannels=2\nir_floats=3\nir_seconds=1\nmax_channels=2\n"
            );
            assert_eq!(
                fs::read(dir.join("ir.f32le")).unwrap(),
                [-0.0, (id + 1) as f32, -0.25]
                    .map(f32::to_le_bytes)
                    .concat()
            );
            for n in 0..4 {
                let before = n as f32 + 0.5;
                assert_eq!(
                    fs::read(dir.join(format!("block-{n}.input.f32le"))).unwrap(),
                    (if id == 0 { before } else { -1.0 })
                        .to_le_bytes()
                        .repeat(1024)
                );
                #[cfg(windows)]
                assert_eq!(
                    fs::read(dir.join(format!("block-{n}.output-before.f32le"))).unwrap(),
                    before.to_le_bytes().repeat(1024)
                );
                assert_eq!(
                    fs::read(dir.join(format!("block-{n}.output.f32le"))).unwrap(),
                    vec![0; 1024 * 4]
                );
                assert_eq!(
                    fs::read(dir.join(format!("block-{n}.gains.f32le"))).unwrap(),
                    [0.25f32, -0.5].map(f32::to_le_bytes).concat()
                );
            }
            assert_eq!(
                fs::read_dir(dir).unwrap().count(),
                if cfg!(windows) { 18 } else { 14 }
            );
        }
    }
}
