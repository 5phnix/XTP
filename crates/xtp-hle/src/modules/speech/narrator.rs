//! Windows voices and in-memory conversion to the title's 48 kHz WAVE format.

use std::ffi::{CString, c_void};
use windows::Media::MediaProperties::{
    AudioEncodingProperties, AudioEncodingQuality, MediaEncodingProfile,
};
use windows::Media::SpeechSynthesis::{SpeechSynthesizer, VoiceGender, VoiceInformation};
use windows::Media::Transcoding::MediaTranscoder;
use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
use windows::Win32::Foundation::{E_BOUNDS, E_FAIL, E_INVALIDARG};
use windows::core::{HSTRING, Result};

use super::{VoiceCallback, VoiceInfo};

fn installed_voices() -> Result<Vec<VoiceInformation>> {
    // On hosts without the speech service even opening the catalog can fail.
    let voices = match SpeechSynthesizer::AllVoices() {
        Ok(voices) => voices,
        Err(error) => {
            tracing::debug!(%error, "Windows speech voices unavailable");
            return Ok(Vec::new());
        }
    };
    (0..voices.Size()?)
        .map(|index| voices.GetAt(index))
        .collect()
}

pub(super) fn open() -> Result<Option<SpeechSynthesizer>> {
    if installed_voices()?.is_empty() {
        return Ok(None);
    }
    SpeechSynthesizer::new().map(Some)
}

pub(super) fn enumerate(context: *mut c_void, callback: VoiceCallback) -> Result<()> {
    let string = |s: HSTRING| CString::new(s.to_string()).map_err(|_| E_INVALIDARG);
    for voice in installed_voices()? {
        let description = string(voice.Description()?)?;
        let name = string(voice.DisplayName()?)?;
        let id = string(voice.Id()?)?;
        let language = string(voice.Language()?)?;
        let info = VoiceInfo {
            description: description.as_ptr(),
            display_name: name.as_ptr(),
            gender: u32::from(voice.Gender()? == VoiceGender::Male),
            id: id.as_ptr(),
            language: language.as_ptr(),
        };
        if !callback(&info, context) {
            break;
        }
    }
    Ok(())
}

pub(super) fn set_voice(engine: &SpeechSynthesizer, id: Option<&str>) -> Result<()> {
    let Some(id) = id else {
        return engine.SetVoice(&SpeechSynthesizer::DefaultVoice()?);
    };
    for voice in installed_voices()? {
        if voice.Id()? == id {
            return engine.SetVoice(&voice);
        }
    }
    Err(E_INVALIDARG.into())
}

pub(super) fn render(engine: &SpeechSynthesizer, words: &str, ssml: bool) -> Result<Vec<u8>> {
    let words = HSTRING::from(words);
    let speech = if ssml {
        engine.SynthesizeSsmlToStreamAsync(&words)?
    } else {
        engine.SynthesizeTextToStreamAsync(&words)?
    }
    .join()?;

    let wave = InMemoryRandomAccessStream::new()?;
    let profile = MediaEncodingProfile::CreateWav(AudioEncodingQuality::Auto)?;
    profile.SetAudio(&AudioEncodingProperties::CreatePcm(48_000, 1, 16)?)?;
    let transcoder = MediaTranscoder::new()?;
    transcoder.SetAlwaysReencode(true)?;
    let prepared = transcoder
        .PrepareStreamTranscodeAsync(&speech, &wave, &profile)?
        .join()?;
    if !prepared.CanTranscode()? {
        tracing::warn!(reason = ?prepared.FailureReason()?, "cannot convert speech to WAVE");
        return Err(E_FAIL.into());
    }
    prepared.TranscodeAsync()?.join()?;

    let size = u32::try_from(wave.Size()?).map_err(|_| E_BOUNDS)?;
    let reader = DataReader::CreateDataReader(&wave.GetInputStreamAt(0)?)?;
    if reader.LoadAsync(size)?.join()? != size {
        return Err(E_FAIL.into());
    }
    let mut bytes = vec![0; size as usize];
    reader.ReadBytes(&mut bytes)?;
    Ok(bytes)
}
