//! What a recording holds, and its encoding: events in a byte stream of
//! LEB128 integers, tags and length-prefixed bytes. The stream is cut into
//! compressed blocks by [`super::store`].

use std::fmt;

/// A call's argument as the replay needs it back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Arg {
    /// A plain value: an integer, a float's bits, a flag.
    Word(u64),
    /// One of our objects (0 for none), by its address in the session.
    Obj(u64),
    /// A GPU (= CPU) address, translated when it lies in memory the
    /// replay could not place where the session had it.
    Addr(u64),
    /// A descriptor handle (an address in a descriptor heap's memory),
    /// translated as addresses are.
    Handle(u64),
    /// A pointer to data the call reads, copied (null pointers are `Word(0)`).
    Blob(Blob),
    /// A pointer the call writes one of our objects of this kind to, and
    /// the object the session got (0 for none).
    Out { kind: u8, made: u64 },
    /// A pointer the call writes `len` plain bytes to; the replay passes
    /// room for them.
    Scratch(u32),
}

/// Bytes a pointer argument points at, and the pointers or values inside
/// them the replay must translate.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Blob {
    pub(crate) bytes: Vec<u8>,
    /// By offset into `bytes`; the value there is the session's.
    pub(crate) fixups: Vec<(u32, Fix)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Fix {
    /// An object pointer.
    Obj,
    /// A GPU address.
    Addr,
    /// A descriptor handle.
    Handle,
    /// A pointer to more copied data.
    Blob(Blob),
}

/// A 4 KiB page of memory as a capture found it.
#[derive(Clone, PartialEq)]
pub(crate) struct Page {
    pub(crate) at: u64,
    /// What it hashed to before (the capture before the submission), for
    /// pages found after a submission.
    pub(crate) before: u64,
    pub(crate) bytes: Vec<u8>,
}

impl fmt::Debug for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Page({:#x}, {} bytes)", self.at, self.bytes.len())
    }
}

/// Bytes per page of the memory captures.
pub(crate) const PAGE: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    /// The device was made, as this interface.
    Device { iid: [u8; 16], version: u32 },
    /// A method call: the object's kind and its table slot, the object,
    /// its arguments (the object excluded) and what it returned. `note`
    /// is what the replay needs besides: the memory a committed resource
    /// got, the base of a heap a placed resource sits in, the start of a
    /// descriptor heap.
    Call {
        kind: u8,
        slot: u16,
        this: u64,
        args: Vec<Arg>,
        ret: u64,
        note: u64,
    },
    /// The title's last reference to an object went.
    Destroy { this: u64 },
    /// Memory an object owns outside title memory (descriptor heaps,
    /// heaps, committed resources, list uploads): its session range.
    Owned { this: u64, at: u64, len: u64 },
    /// The recorded frames start here: memory is the snapshot's
    /// (`memory.bin`) from now on, after frame `frame` was presented.
    Window { frame: u64 },
    /// A fence's value when the window started.
    Fence { this: u64, value: u64 },
    /// Pages the CPU wrote since the last capture (`gpu: false`, before a
    /// submission), or that changed while a submission ran (`gpu: true`:
    /// what our copies of GPU results wrote back, and any CPU write
    /// meanwhile).
    Pages { gpu: bool, pages: Vec<Page> },
    /// Frame `frame` was presented (after its present call).
    Presented { frame: u64 },
    /// The recording ends.
    End,
}

/// Appends LEB128 integers, tags and bytes.
#[derive(Default)]
pub(crate) struct Writer {
    pub(crate) out: Vec<u8>,
}

impl Writer {
    pub(crate) fn uint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.out.push(byte);
                return;
            }
            self.out.push(byte | 0x80);
        }
    }

    fn bytes(&mut self, b: &[u8]) {
        self.uint(b.len() as u64);
        self.out.extend_from_slice(b);
    }

    fn blob(&mut self, b: &Blob) {
        self.bytes(&b.bytes);
        self.uint(b.fixups.len() as u64);
        for (at, fix) in &b.fixups {
            self.uint(u64::from(*at));
            match fix {
                Fix::Obj => self.uint(0),
                Fix::Addr => self.uint(1),
                Fix::Handle => self.uint(2),
                Fix::Blob(inner) => {
                    self.uint(3);
                    self.blob(inner);
                }
            }
        }
    }

    fn arg(&mut self, a: &Arg) {
        match a {
            Arg::Word(v) => {
                self.uint(0);
                self.uint(*v);
            }
            Arg::Obj(v) => {
                self.uint(1);
                self.uint(*v);
            }
            Arg::Addr(v) => {
                self.uint(2);
                self.uint(*v);
            }
            Arg::Handle(v) => {
                self.uint(3);
                self.uint(*v);
            }
            Arg::Blob(b) => {
                self.uint(4);
                self.blob(b);
            }
            Arg::Out { kind, made } => {
                self.uint(5);
                self.uint(u64::from(*kind));
                self.uint(*made);
            }
            Arg::Scratch(len) => {
                self.uint(6);
                self.uint(u64::from(*len));
            }
        }
    }

    pub(crate) fn event(&mut self, e: &Event) {
        match e {
            Event::Device { iid, version } => {
                self.uint(0);
                self.out.extend_from_slice(iid);
                self.uint(u64::from(*version));
            }
            Event::Call {
                kind,
                slot,
                this,
                args,
                ret,
                note,
            } => {
                self.uint(1);
                self.uint(u64::from(*kind));
                self.uint(u64::from(*slot));
                self.uint(*this);
                self.uint(args.len() as u64);
                for a in args {
                    self.arg(a);
                }
                self.uint(*ret);
                self.uint(*note);
            }
            Event::Destroy { this } => {
                self.uint(2);
                self.uint(*this);
            }
            Event::Owned { this, at, len } => {
                self.uint(3);
                self.uint(*this);
                self.uint(*at);
                self.uint(*len);
            }
            Event::Window { frame } => {
                self.uint(4);
                self.uint(*frame);
            }
            Event::Fence { this, value } => {
                self.uint(5);
                self.uint(*this);
                self.uint(*value);
            }
            Event::Pages { gpu, pages } => {
                self.uint(6);
                self.uint(u64::from(*gpu));
                self.uint(pages.len() as u64);
                for p in pages {
                    self.uint(p.at);
                    self.uint(p.before);
                    self.bytes(&p.bytes);
                }
            }
            Event::Presented { frame } => {
                self.uint(7);
                self.uint(*frame);
            }
            Event::End => self.uint(8),
        }
    }
}

/// Reads what [`Writer`] wrote.
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

/// A stream that ends inside an event, or holds a tag we do not know.
#[derive(Debug)]
pub(crate) struct Corrupt(pub(crate) usize);

impl fmt::Display for Corrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "recording corrupt at byte {}", self.0)
    }
}

impl std::error::Error for Corrupt {}

type Read<T> = Result<T, Corrupt>;

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, at: 0 }
    }

    #[cfg(test)]
    pub(crate) fn done(&self) -> bool {
        self.at >= self.data.len()
    }

    /// Bytes not read yet.
    pub(crate) fn rest(&self) -> usize {
        self.data.len() - self.at
    }

    fn uint(&mut self) -> Read<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let &byte = self.data.get(self.at).ok_or(Corrupt(self.at))?;
            self.at += 1;
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(Corrupt(self.at))
    }

    fn small<T: TryFrom<u64>>(&mut self) -> Read<T> {
        let at = self.at;
        T::try_from(self.uint()?).map_err(|_| Corrupt(at))
    }

    fn take(&mut self, len: usize) -> Read<&'a [u8]> {
        let end = self.at.checked_add(len).ok_or(Corrupt(self.at))?;
        let b = self.data.get(self.at..end).ok_or(Corrupt(self.at))?;
        self.at = end;
        Ok(b)
    }

    fn bytes(&mut self) -> Read<Vec<u8>> {
        let len = self.small::<usize>()?;
        Ok(self.take(len)?.to_vec())
    }

    fn blob(&mut self) -> Read<Blob> {
        let bytes = self.bytes()?;
        let n = self.small::<usize>()?;
        let mut fixups = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            let at = self.small::<u32>()?;
            let fix = match self.uint()? {
                0 => Fix::Obj,
                1 => Fix::Addr,
                2 => Fix::Handle,
                3 => Fix::Blob(self.blob()?),
                _ => return Err(Corrupt(self.at)),
            };
            if at as usize + 8 > bytes.len() {
                return Err(Corrupt(self.at));
            }
            fixups.push((at, fix));
        }
        Ok(Blob { bytes, fixups })
    }

    fn arg(&mut self) -> Read<Arg> {
        Ok(match self.uint()? {
            0 => Arg::Word(self.uint()?),
            1 => Arg::Obj(self.uint()?),
            2 => Arg::Addr(self.uint()?),
            3 => Arg::Handle(self.uint()?),
            4 => Arg::Blob(self.blob()?),
            5 => Arg::Out {
                kind: self.small()?,
                made: self.uint()?,
            },
            6 => Arg::Scratch(self.small()?),
            _ => return Err(Corrupt(self.at)),
        })
    }

    pub(crate) fn event(&mut self) -> Read<Event> {
        Ok(match self.uint()? {
            0 => Event::Device {
                iid: self.take(16)?.try_into().map_err(|_| Corrupt(self.at))?,
                version: self.small()?,
            },
            1 => {
                let kind = self.small()?;
                let slot = self.small()?;
                let this = self.uint()?;
                let n = self.small::<usize>()?;
                let mut args = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    args.push(self.arg()?);
                }
                Event::Call {
                    kind,
                    slot,
                    this,
                    args,
                    ret: self.uint()?,
                    note: self.uint()?,
                }
            }
            2 => Event::Destroy { this: self.uint()? },
            3 => Event::Owned {
                this: self.uint()?,
                at: self.uint()?,
                len: self.uint()?,
            },
            4 => Event::Window {
                frame: self.uint()?,
            },
            5 => Event::Fence {
                this: self.uint()?,
                value: self.uint()?,
            },
            6 => {
                let gpu = self.uint()? != 0;
                let n = self.small::<usize>()?;
                let mut pages = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    pages.push(Page {
                        at: self.uint()?,
                        before: self.uint()?,
                        bytes: self.bytes()?,
                    });
                }
                Event::Pages { gpu, pages }
            }
            7 => Event::Presented {
                frame: self.uint()?,
            },
            8 => Event::End,
            _ => return Err(Corrupt(self.at)),
        })
    }
}
