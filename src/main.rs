use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use futures_util::{SinkExt, StreamExt};
use hound::{SampleFormat, WavSpec, WavWriter};
use kokoros::tts::koko::TTSKoko;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Config {
    ws_base_url: String,
    conversation_id: String,
    user_id: String,
    token: Option<String>,
    model_path: String,
    voices_path: String,
    voice: String,
    language: String,
    speed: f32,
}

impl Config {
    fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        Ok(Config {
            ws_base_url: std::env::var("WS_URL")
                .unwrap_or_else(|_| "wss://api.ramble.my".to_string()),
            conversation_id: std::env::var("CONVERSATION_ID")
                .unwrap_or_else(|_| "test".to_string()),
            user_id: std::env::var("USER_ID")
                .unwrap_or_else(|_| "tts-server".to_string()),
            token: std::env::var("TOKEN").ok(),
            model_path: std::env::var("MODEL_PATH")
                .unwrap_or_else(|_| "checkpoints/kokoro-v1.0.onnx".to_string()),
            voices_path: std::env::var("VOICES_PATH")
                .unwrap_or_else(|_| "data/voices-v1.0.bin".to_string()),
            voice: std::env::var("VOICE")
                .unwrap_or_else(|_| "af_heart".to_string()),
            language: std::env::var("LANGUAGE")
                .unwrap_or_else(|_| "en-us".to_string()),
            speed: std::env::var("SPEED")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1.0),
        })
    }

    fn connection_url(&self) -> String {
        let mut url = format!(
            "{}/ws/{}?type=tts-server&userId={}",
            self.ws_base_url, self.conversation_id, self.user_id
        );
        if let Some(token) = &self.token {
            url.push_str(&format!("&token={token}"));
        }
        url
    }
}

// ---------------------------------------------------------------------------
// WebSocket message envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct WsMessage {
    id: String,
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<Value>,
    payload: Value,
}

impl WsMessage {
    fn new(msg_type: &str, to: Option<Value>, payload: Value) -> Self {
        WsMessage {
            id: Uuid::new_v4().to_string(),
            msg_type: msg_type.to_string(),
            from: None,
            to,
            payload,
        }
    }
}

// ---------------------------------------------------------------------------
// Audio helpers
// ---------------------------------------------------------------------------

/// Encode 24 kHz mono f32 PCM samples as a WAV byte buffer (PCM-16).
fn encode_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    let spec = WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut buf = Cursor::new(Vec::new());
    let mut writer = WavWriter::new(&mut buf, spec)?;
    for &s in samples {
        writer.write_sample((s * 32767.0).clamp(-32768.0, 32767.0) as i16)?;
    }
    writer.finalize()?;
    Ok(buf.into_inner())
}

// ---------------------------------------------------------------------------
// TTS synthesis → WebSocket send
// ---------------------------------------------------------------------------

async fn synthesize_and_send(
    tts: Arc<TTSKoko>,
    tx: mpsc::Sender<String>,
    text: String,
    reply_to_id: String,
    // Route back to the specific sender wsId, else broadcast to all runtimes.
    sender_ws_id: Option<String>,
    voice: String,
    language: String,
    speed: f32,
) -> Result<()> {
    info!(
        "Synthesizing ({} chars): {:?}",
        text.len(),
        &text[..text.len().min(80)]
    );

    // Split into sentence-level speech chunks (≤30 words each).
    let chunks = tts.split_text_into_speech_chunks(&text, 30);
    let total = chunks.len();

    for (i, chunk) in chunks.into_iter().enumerate() {
        if chunk.trim().is_empty() {
            continue;
        }
        let is_final = i == total - 1;

        // Run blocking ONNX inference on the thread-pool.
        let tts_ref = Arc::clone(&tts);
        let chunk_text = chunk.clone();
        let voice_ref = voice.clone();
        let lang_ref = language.clone();

        let samples = tokio::task::spawn_blocking(move || {
            tts_ref
                .tts_raw_audio(
                    &chunk_text,
                    &lang_ref,
                    &voice_ref,
                    speed,
                    None,
                    None,
                    None,
                    None,
                )
                .map_err(|e| e.to_string())
        })
        .await
        .context("TTS thread panicked")?
        .map_err(|e| anyhow::anyhow!("TTS synthesis: {e}"))?;

        if samples.is_empty() {
            continue;
        }

        let duration_ms = (samples.len() as f64 / 24_000.0 * 1000.0) as u64;
        let wav = encode_wav(&samples, 24_000)?;
        let audio_b64 = BASE64.encode(&wav);

        // Routing: specific wsId > all runtime clients.
        let to = match &sender_ws_id {
            Some(id) => serde_json::json!({ "id": id }),
            None => serde_json::json!({ "type": "runtime" }),
        };

        let payload = serde_json::json!({
            "inReplyToMessageId": reply_to_id,
            "chunkIndex": i,
            "isFinal": is_final,
            "text": chunk,
            "audioData": audio_b64,
            "sampleRate": 24_000,
            "durationMs": duration_ms,
        });

        let msg = WsMessage::new("tts_audio", Some(to), payload);
        tx.send(serde_json::to_string(&msg)?)
            .await
            .context("Send channel closed")?;

        info!(
            "  chunk {}/{} sent ({} ms, {} bytes WAV)",
            i + 1,
            total,
            duration_ms,
            wav.len()
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// WebSocket connection loop (reconnecting)
// ---------------------------------------------------------------------------

async fn run(config: Arc<Config>, tts: Arc<TTSKoko>) {
    let mut backoff = Duration::from_secs(1);

    loop {
        let url = config.connection_url();
        info!("Connecting to {url}");

        match connect_async(&url).await {
            Err(e) => {
                error!("Connection failed: {e}. Retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
            Ok((ws, _)) => {
                info!("WebSocket connected");
                backoff = Duration::from_secs(1); // reset on clean connect

                let (mut sink, mut stream) = ws.split();

                // Outbound channel — decouples synthesis tasks from the sink.
                let (tx, mut rx) = mpsc::channel::<String>(64);

                tokio::spawn(async move {
                    while let Some(json) = rx.recv().await {
                        if let Err(e) = sink.send(Message::Text(json.into())).await {
                            error!("WS write error: {e}");
                            break;
                        }
                    }
                });

                while let Some(result) = stream.next().await {
                    match result {
                        Ok(Message::Text(raw)) => {
                            let text = raw.to_string();
                            let msg: WsMessage = match serde_json::from_str(&text) {
                                Ok(m) => m,
                                Err(e) => {
                                    warn!(
                                        "Unparseable message ({e}): {}",
                                        &text[..text.len().min(120)]
                                    );
                                    continue;
                                }
                            };

                            match msg.msg_type.as_str() {
                                "connected" => {
                                    let ws_id = msg
                                        .payload
                                        .get("wsId")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("?");
                                    info!("Registered as tts-server  wsId={ws_id}");
                                }

                                "ping" => {
                                    let pong =
                                        WsMessage::new("pong", None, serde_json::json!({}));
                                    let _ = tx
                                        .send(serde_json::to_string(&pong).unwrap())
                                        .await;
                                }

                                // Explicit TTS request routed to this agent.
                                "tts_request" => {
                                    let text_to_speak = msg
                                        .payload
                                        .get("text")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let reply_to = msg.id.clone();
                                    let sender_id = msg
                                        .from
                                        .as_ref()
                                        .and_then(|f| f.get("id"))
                                        .and_then(|v| v.as_str())
                                        .map(String::from);

                                    if text_to_speak.is_empty() {
                                        continue;
                                    }

                                    let (tts, tx) = (Arc::clone(&tts), tx.clone());
                                    let (v, l, sp) = (
                                        config.voice.clone(),
                                        config.language.clone(),
                                        config.speed,
                                    );
                                    tokio::spawn(async move {
                                        if let Err(e) = synthesize_and_send(
                                            tts, tx, text_to_speak, reply_to, sender_id, v, l, sp,
                                        )
                                        .await
                                        {
                                            error!("TTS task error: {e}");
                                        }
                                    });
                                }

                                // Assistant response broadcast — TTS the reply text.
                                "assistant_response" => {
                                    let text_to_speak = msg
                                        .payload
                                        .get("quickResponse")
                                        .and_then(|qr| qr.get("response"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let reply_to = msg
                                        .payload
                                        .get("messageId")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(&msg.id)
                                        .to_string();

                                    if text_to_speak.is_empty() {
                                        continue;
                                    }

                                    let (tts, tx) = (Arc::clone(&tts), tx.clone());
                                    let (v, l, sp) = (
                                        config.voice.clone(),
                                        config.language.clone(),
                                        config.speed,
                                    );
                                    tokio::spawn(async move {
                                        if let Err(e) = synthesize_and_send(
                                            tts, tx, text_to_speak, reply_to, None, v, l, sp,
                                        )
                                        .await
                                        {
                                            error!("TTS task error: {e}");
                                        }
                                    });
                                }

                                // Ignore presence, chunk_result, and anything else.
                                _ => {}
                            }
                        }

                        Ok(Message::Close(_)) => {
                            info!("Server closed connection");
                            break;
                        }

                        Ok(Message::Ping(_)) => {
                            // tokio-tungstenite sends Pong automatically.
                        }

                        Err(e) => {
                            error!("WebSocket read error: {e}");
                            break;
                        }

                        _ => {}
                    }
                }

                warn!("Disconnected. Reconnecting in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tts_server=info".parse().unwrap()),
        )
        .init();

    let config = Arc::new(Config::from_env().context("Config error")?);

    info!("Loading Kokoro model from {}", config.model_path);
    let tts = Arc::new(TTSKoko::new(&config.model_path, &config.voices_path).await);
    info!(
        "TTS engine ready  voice={} lang={}",
        config.voice, config.language
    );

    run(config, tts).await;

    Ok(())
}
