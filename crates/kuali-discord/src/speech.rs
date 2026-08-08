//! Prerecorded consent notice played by Kuali in the voice channel.

use std::sync::Arc;

use songbird::input::{
    codecs::{get_codec_registry, get_probe},
    Input,
};

const CONSENT_AUDIO: &[u8] = include_bytes!("../../../assets/aviso-legal-2x.mp3");

/// The MP3 is preprocessed at 2× speed and embedded in the executable, avoiding
/// dependencies on `say`, ffmpeg, or installed system voices.
pub async fn load_consent_audio() -> Result<Arc<[u8]>, String> {
    let audio: Arc<[u8]> = CONSENT_AUDIO.into();

    // Failing here provides a startup diagnosis instead of letting Songbird
    // discover an unsupported format silently during a call.
    let input: Input = Arc::clone(&audio).into();
    input
        .make_playable_async(get_codec_registry(), get_probe())
        .await
        .map_err(|error| format!("no pude preparar el aviso hablado: {error}"))?;

    Ok(audio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bundled_consent_announcement_is_a_playable_mp3() {
        let audio = load_consent_audio()
            .await
            .expect("load consent announcement");

        assert!(audio.len() > 1_000);
        assert_eq!(&audio[..3], b"ID3");
    }
}
