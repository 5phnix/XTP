//! Windows voices and in-memory conversion to the title's 48 kHz WAVE format.

use std::ffi::{CString, c_void};
use windows::Data::Xml::Dom::{XmlDocument, XmlLoadSettings};
use windows::Media::MediaProperties::{
    AudioEncodingProperties, AudioEncodingQuality, MediaEncodingProfile,
};
use windows::Media::SpeechSynthesis::{
    SpeechSynthesisStream, SpeechSynthesizer, VoiceGender, VoiceInformation,
};
use windows::Media::Transcoding::MediaTranscoder;
use windows::Storage::Streams::{DataReader, InMemoryRandomAccessStream};
use windows::Win32::Foundation::{E_BOUNDS, E_FAIL, E_INVALIDARG};
use windows::core::{HRESULT, HSTRING, Result, h};

use super::{SILENCE, VoiceCallback, VoiceInfo};

// WinRT reports malformed SSML as a FormatException.
const COR_E_FORMAT: HRESULT = HRESULT(0x8013_1537_u32 as i32);

fn installed_voices() -> Result<Vec<VoiceInformation>> {
    // On hosts without the speech service even opening the catalog can fail.
    let voices = match SpeechSynthesizer::AllVoices() {
        Ok(voices) => voices,
        Err(error) => {
            tracing::warn!(%error, "Windows speech voices unavailable");
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
    if !ssml && words.trim().is_empty() {
        return Ok(SILENCE.to_vec());
    }
    let words = HSTRING::from(words);
    let speech = if ssml {
        offline_ssml(words).and_then(|markup| engine.SynthesizeSsmlToStreamAsync(&markup))
    } else {
        engine.SynthesizeTextToStreamAsync(&words)
    }
    .and_then(|op| op.join());
    match speech {
        Err(error) if ssml && error.code() == COR_E_FORMAT => Err(error),
        result => Ok(result.and_then(to_wave).unwrap_or_else(|error| {
            tracing::warn!(%error, "could not render Windows speech; using silence");
            SILENCE.to_vec()
        })),
    }
}

/// Use the audio element's fallback content. The host receives no audio source to load.
fn offline_ssml(words: HSTRING) -> Result<HSTRING> {
    let doc = XmlDocument::new()?;
    let settings = XmlLoadSettings::new()?;
    settings.SetProhibitDtd(true)?;
    settings.SetResolveExternals(false)?;
    settings.SetElementContentWhiteSpace(true)?;
    doc.LoadXmlWithSettings(&words, &settings)
        .map_err(|_| COR_E_FORMAT)?;
    let nodes = doc.SelectNodes(h!("//*[local-name()='audio']"))?;
    let count = nodes.Length()?;
    if count == 0 {
        return Ok(words);
    }
    // Unwrap nested audio first. A desc child describes the recording and is
    // not part of the fallback narration.
    for index in (0..count).rev() {
        let audio = nodes.Item(index)?;
        let parent = audio.ParentNode()?;
        let fallback = audio.SelectNodes(h!("node()[local-name() != 'desc']"))?;
        for i in 0..fallback.Length()? {
            parent.InsertBefore(&fallback.Item(i)?, &audio)?;
        }
        parent.RemoveChild(&audio)?;
    }
    doc.GetXml()
}

fn to_wave(speech: SpeechSynthesisStream) -> Result<Vec<u8>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_references_keep_only_the_fallback_in_document_order() {
        let markup = h!(
            "<speak xmlns='http://www.w3.org/2001/10/synthesis' xmlns:v='http://www.w3.org/2001/10/synthesis'>one <v:audio src='first.wav'><desc>recording</desc><s>two</s> <audio src='second.wav'>three</audio></v:audio> four<audio src='last.wav'/></speak>"
        );
        let result = offline_ssml(markup.clone()).unwrap().to_string();
        assert!(result.contains("one <s>two</s> three four"), "{result}");
        assert!(!result.contains(".wav") && !result.contains("recording"));
    }
}
