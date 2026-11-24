//! Accessibility covers caption preferences, high contrast and speech synthesis.
//! Synthesized speech streams own their WAVE data until the title closes them.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod captions;
#[cfg(windows)]
mod narrator;

use std::ffi::{c_char, c_void};

use super::xasync::{E_INVALIDARG, E_NOT_SUFFICIENT_BUFFER};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, text,
};

/// Closed captions, high contrast, speech to text and synthesis.
pub const PROVIDER: Guid = Guid::new(
    0x3e24_1977,
    0x6237,
    0x41e9,
    [0x85, 0x59, 0x10, 0x2c, 0x2d, 0x99, 0x83, 0xf1],
);
/// Unchanged since 2020.
pub const IID: Guid = Guid::new(
    0xd722_b373,
    0x8c4d,
    0x4692,
    [0x9e, 0x51, 0xd6, 0xad, 0x9b, 0x37, 0xaa, 0x7d],
);

pub static OBJECT: Object = Object::new("accessibility", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID, &OBJECT)];

/// A WAVE file of 16 kHz mono 16-bit PCM with no samples: RIFF size 36,
/// a 16-byte format chunk (PCM, 1 channel, 16,000 Hz, 32,000 bytes/s,
/// block 2, 16 bits), an empty data chunk.
const SILENCE: [u8; 44] =
    *b"RIFF\x24\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\x3e\0\0\0\x7d\0\0\x02\0\x10\0data\0\0\0\0";

type This = *const Object;
const STREAM_NOT_FOUND: HRESULT = 0x8007_0490_u32 as i32; // HRESULT_FROM_WIN32(ERROR_NOT_FOUND)

enum Synthesizer {
    Silent,
    #[cfg(windows)]
    Windows(windows::Media::SpeechSynthesis::SpeechSynthesizer),
}

#[repr(C)]
struct VoiceInfo {
    description: *const c_char,
    display_name: *const c_char,
    gender: u32,
    id: *const c_char,
    language: *const c_char,
}

type VoiceCallback = extern "system" fn(&VoiceInfo, *mut c_void) -> bool;

extern "system" fn enumerate_voices(
    _: This,
    _context: *mut c_void,
    callback: Option<VoiceCallback>,
) -> HRESULT {
    let Some(_callback) = callback else {
        return E_INVALIDARG;
    };
    #[cfg(windows)]
    if let Err(e) = narrator::enumerate(_context, _callback) {
        return e.code().0;
    }
    S_OK
}
extern "system" fn create(_: This, out: Option<&mut *mut Synthesizer>) -> HRESULT {
    let Some(out) = out else { return E_INVALIDARG };
    *out = std::ptr::null_mut();
    #[cfg(windows)]
    let synth = match narrator::open() {
        Ok(Some(engine)) => Synthesizer::Windows(engine),
        Ok(None) => Synthesizer::Silent,
        Err(error) => {
            tracing::warn!(%error, "could not open Windows speech synthesizer; using silence");
            Synthesizer::Silent
        }
    };
    #[cfg(not(windows))]
    let synth = Synthesizer::Silent;
    *out = Box::into_raw(Box::new(synth));
    S_OK
}
extern "system" fn close<T>(_: This, handle: *mut T) -> HRESULT {
    if !handle.is_null() {
        // SAFETY: create and create_stream each hand a Box to the title, closed once after use.
        drop(unsafe { Box::from_raw(handle) });
    }
    S_OK
}
fn set_voice(synth: Option<&Synthesizer>, _id: Option<&str>) -> HRESULT {
    match synth {
        None => E_INVALIDARG,
        Some(Synthesizer::Silent) => S_OK,
        #[cfg(windows)]
        Some(Synthesizer::Windows(engine)) => {
            narrator::set_voice(engine, _id).map_or_else(|e| e.code().0, |()| S_OK)
        }
    }
}
extern "system" fn set_default_voice(_: This, synth: Option<&Synthesizer>) -> HRESULT {
    set_voice(synth, None)
}
extern "system" fn set_custom_voice(
    _: This,
    synth: Option<&Synthesizer>,
    id: *const c_char,
) -> HRESULT {
    let Some(id) = text(id) else {
        return E_INVALIDARG;
    };
    set_voice(synth, Some(&id))
}
extern "system" fn create_stream<const SSML: bool>(
    _: This,
    synth: Option<&Synthesizer>,
    words: *const c_char,
    out: Option<&mut *mut Vec<u8>>,
) -> HRESULT {
    let Some(out) = out else { return E_INVALIDARG };
    *out = std::ptr::null_mut();
    let (Some(synth), Some(_words)) = (synth, text(words)) else {
        return E_INVALIDARG;
    };
    let wav = match synth {
        Synthesizer::Silent => SILENCE.to_vec(),
        #[cfg(windows)]
        Synthesizer::Windows(engine) => match narrator::render(engine, &_words, SSML) {
            Ok(wav) => wav,
            Err(e) => return e.code().0,
        },
    };
    *out = Box::into_raw(Box::new(wav));
    S_OK
}
extern "system" fn stream_size(_: This, stream: Option<&Vec<u8>>, out: *mut usize) -> HRESULT {
    stream.map_or(STREAM_NOT_FOUND, |wav| put(out, wav.len()))
}
extern "system" fn stream_data(
    _: This,
    stream: Option<&Vec<u8>>,
    size: usize,
    buffer: *mut u8,
    used: *mut usize,
) -> HRESULT {
    let Some(wav) = stream else {
        return STREAM_NOT_FOUND;
    };
    if buffer.is_null() || size < wav.len() {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe { buffer.copy_from_nonoverlapping(wav.as_ptr(), wav.len()) };
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(wav.len()) };
    }
    S_OK
}

static VTABLE: Methods<22> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    captions::properties as *const (),    // 0x18 closed captions
    captions::set_enabled as *const (),   // 0x20
    captions::high_contrast as *const (), // 0x28 high contrast
    missing::<6> as *const (),            // 0x30 speech to text
    missing::<7> as *const (),            // 0x38
    enumerate_voices as *const (),        // 0x40
    create as *const (),                  // 0x48
    close::<Synthesizer> as *const (),    // 0x50
    set_default_voice as *const (),       // 0x58
    set_custom_voice as *const (),        // 0x60
    create_stream::<false> as *const (),  // 0x68 from text
    close::<Vec<u8>> as *const (),        // 0x70 a stream
    stream_size as *const (),             // 0x78
    stream_data as *const (),             // 0x80
    missing::<17> as *const (),           // 0x88 speech to text
    missing::<18> as *const (),           // 0x90
    missing::<19> as *const (),           // 0x98
    missing::<20> as *const (),           // 0xa0
    create_stream::<true> as *const (),   // 0xa8 from SSML
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;
    use std::ffi::CStr;

    type Stream =
        extern "system" fn(This, &Synthesizer, *const c_char, *mut *mut Vec<u8>) -> HRESULT;

    fn wave(synth: &Synthesizer, slot: usize, words: &CStr) -> Vec<u8> {
        let this: This = &OBJECT;
        type Size = extern "system" fn(This, *mut Vec<u8>, *mut usize) -> HRESULT;
        type Data = extern "system" fn(This, *mut Vec<u8>, usize, *mut u8, *mut usize) -> HRESULT;
        let (mut stream, mut size) = (std::ptr::null_mut(), 0);
        assert_eq!(
            method::<Stream>(&OBJECT, slot)(this, synth, words.as_ptr(), &mut stream),
            S_OK
        );
        assert_eq!(method::<Size>(&OBJECT, 0x78)(this, stream, &mut size), S_OK);
        let mut wav = vec![0u8; size];
        let mut used = 0;
        let data = method::<Data>(&OBJECT, 0x80);
        assert_eq!(
            data(this, stream, size - 1, wav.as_mut_ptr(), &mut used),
            E_NOT_SUFFICIENT_BUFFER
        );
        assert_eq!(data(this, stream, size, wav.as_mut_ptr(), &mut used), S_OK);
        assert_eq!(used, size);
        type Close = extern "system" fn(This, *mut Vec<u8>) -> HRESULT;
        assert_eq!(method::<Close>(&OBJECT, 0x70)(this, stream), S_OK);
        wav
    }

    #[test]
    fn no_voice_returns_the_empty_wave_for_text_and_ssml() {
        for slot in [0x68, 0xa8] {
            assert_eq!(wave(&Synthesizer::Silent, slot, c"test"), SILENCE);
        }
    }

    #[test]
    fn null_stream_is_not_found() {
        assert_eq!(stream_size(&OBJECT, None, &mut 0), STREAM_NOT_FOUND);
        assert_eq!(
            stream_data(&OBJECT, None, 0, std::ptr::null_mut(), std::ptr::null_mut()),
            STREAM_NOT_FOUND
        );
    }

    #[cfg(windows)]
    #[test]
    fn installed_voice_speaks_text_and_ssml() {
        use std::ffi::CString;
        // Stop after the first voice. The title can keep a copy of its ID.
        extern "system" fn first(info: &VoiceInfo, context: *mut c_void) -> bool {
            // SAFETY: context points to the test's output; the voice strings live through this call.
            unsafe {
                *context.cast::<Option<(CString, String)>>() = Some((
                    CStr::from_ptr(info.id).to_owned(),
                    CStr::from_ptr(info.language).to_string_lossy().into_owned(),
                ));
            }
            false
        }
        let mut voice: Option<(CString, String)> = None;
        assert_eq!(
            enumerate_voices(&OBJECT, (&raw mut voice).cast(), Some(first)),
            S_OK
        );
        let mut handle = std::ptr::null_mut();
        assert_eq!(create(&OBJECT, Some(&mut handle)), S_OK);
        // SAFETY: create returned a live handle which this test owns until close.
        let synth = unsafe { &*handle };
        for blank in [c"", c" \t\r\n"] {
            assert_eq!(wave(synth, 0x68, blank), SILENCE);
        }
        let Some((id, language)) = voice else {
            assert_eq!(wave(synth, 0x68, c"test"), SILENCE);
            assert_eq!(close(&OBJECT, handle), S_OK);
            xtp_testkit::skip("no-voice");
            return;
        };
        assert_eq!(set_custom_voice(&OBJECT, Some(synth), id.as_ptr()), S_OK);
        assert_eq!(
            set_custom_voice(&OBJECT, Some(synth), c"missing-test-voice".as_ptr()),
            E_INVALIDARG
        );
        let ssml = CString::new(format!(r#"<speak version="1.0" xmlns="http://www.w3.org/2001/10/synthesis" xml:lang="{language}"><audio src="unused.wav"><s>test</s></audio></speak>"#)).unwrap();
        let spoken_ssml = wave(synth, 0xa8, &ssml);
        let mut invalid = std::ptr::null_mut();
        assert_ne!(
            method::<Stream>(&OBJECT, 0xa8)(&OBJECT, synth, c"<speak>".as_ptr(), &mut invalid),
            S_OK
        );
        assert!(invalid.is_null());
        assert_eq!(set_default_voice(&OBJECT, Some(synth)), S_OK);
        let spoken_text = wave(synth, 0x68, c"test");
        // Simulate losing the host service after the title has acquired its handle.
        let Synthesizer::Windows(engine) = synth else {
            panic!("expected a Windows voice")
        };
        engine.Close().unwrap();
        assert_eq!(wave(synth, 0x68, c"test"), SILENCE);
        assert_eq!(wave(synth, 0xa8, &ssml), SILENCE);
        assert_eq!(close(&OBJECT, handle), S_OK);
        for wav in [spoken_text, spoken_ssml] {
            let size = |bytes: &[u8]| u32::from_le_bytes(bytes.try_into().unwrap()) as usize;
            assert_eq!(&wav[..4], b"RIFF");
            assert_eq!(size(&wav[4..8]) + 8, wav.len());
            assert_eq!(&wav[8..12], b"WAVE");
            let mut chunks = &wav[12..];
            let (mut format, mut samples) = (None, None);
            while !chunks.is_empty() {
                let len = size(&chunks[4..8]);
                let payload = &chunks[8..8 + len];
                match &chunks[..4] {
                    b"fmt " => format = Some(payload),
                    b"data" => samples = Some(payload),
                    _ => (),
                }
                chunks = &chunks[8 + len + (len % 2)..];
            }
            let fmt = format.unwrap();
            assert_eq!(u16::from_le_bytes(fmt[0..2].try_into().unwrap()), 1); // PCM
            assert_eq!(size(&fmt[4..8]), 48_000);
            assert_eq!(size(&fmt[8..12]), 96_000);
            assert_eq!(u16::from_le_bytes(fmt[12..14].try_into().unwrap()), 2);
            assert_eq!(u16::from_le_bytes(fmt[2..4].try_into().unwrap()), 1);
            assert_eq!(u16::from_le_bytes(fmt[14..16].try_into().unwrap()), 16);
            assert!(samples.unwrap().iter().any(|&byte| byte != 0));
        }
    }
}
