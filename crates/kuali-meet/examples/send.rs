//! Sends Kuali a WAV as though it came from a browser meeting.
//!
//! This distinguishes a Kuali receiver failure from an extension sender failure.
//! Successful transcription points investigation toward the browser.
//!
//! Usage: send <audio.wav> [name] [port]
//! The WAV must contain 16 kHz mono Float32 audio matching the wire format.

use std::time::Duration;

use futures_util::SinkExt;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;

/// Sends realistic chunks so the segmenter receives conversational timing rather
/// than the entire recording at once.
const CHUNK_MS: usize = 100;
const SAMPLE_RATE: usize = 16_000;

fn read_wav_f32(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("failed to read WAV file");
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let size = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().unwrap()) as usize;
        if &bytes[i..i + 4] == b"data" {
            return bytes[i + 8..(i + 8 + size).min(bytes.len())]
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                .collect();
        }
        i += 8 + size + (size & 1);
    }
    panic!("WAV file has no data chunk");
}

/// Named `capture.v1` frame matching extension output.
fn encode_named(speaker_index: u32, ts_ms: f64, name: &str, pcm: &[f32]) -> Vec<u8> {
    let name = name.as_bytes();
    let padded = (name.len() + 3) & !3;
    let mut out = Vec::with_capacity(16 + padded + pcm.len() * 4);
    out.extend_from_slice(&(speaker_index | 0x8000_0000).to_le_bytes());
    out.extend_from_slice(&ts_ms.to_le_bytes());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name);
    out.resize(16 + padded, 0);
    for sample in pcm {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("missing WAV file path");
    let name = args.get(2).cloned().unwrap_or_else(|| "Ana".to_string());
    let port: u16 = args.get(3).and_then(|p| p.parse().ok()).unwrap_or(9099);

    let samples = read_wav_f32(path);
    println!(
        "{path}: {:.1} s of audio, speaker `{name}`",
        samples.len() as f32 / SAMPLE_RATE as f32
    );

    let url = format!(
        "ws://127.0.0.1:{port}/ingest?platform=google_meet&native_meeting_id=prueba-kuali&api_key=x&language=es"
    );
    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("is Kuali listening on that port?");
    let (mut ws, _) = tokio_tungstenite::client_async(&url, stream)
        .await
        .expect("handshake failed");
    println!("connected; sending audio…");

    let chunk = SAMPLE_RATE * CHUNK_MS / 1_000;
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64;

    for (i, block) in samples.chunks(chunk).enumerate() {
        let ts = start + (i * CHUNK_MS) as f64;
        let frame = encode_named(0, ts, &name, block);
        ws.send(Message::Binary(frame.into())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(CHUNK_MS as u64)).await;
    }

    // Allow silence to close the turn before disconnecting; an immediate close
    // would test disconnection rather than normal segmentation.
    println!("audio sent; waiting for the speaker turn to close…");
    tokio::time::sleep(Duration::from_secs(4)).await;
    ws.close(None).await.unwrap();
    println!("done. Check the transcript in Kuali.");
}
