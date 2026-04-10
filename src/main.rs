use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use regex::Regex;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use futures_util::{stream::FuturesOrdered, SinkExt, StreamExt};
use hound::{SampleFormat, WavSpec, WavWriter};
use kokoros::tts::koko::TTSKoko;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore};
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
    /// How many requests can be synthesised in parallel.
    /// Each worker loads one copy of the model into memory (~310 MB each).
    workers: usize,
}

impl Config {
    fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        Ok(Config {
            ws_base_url: std::env::var("WS_URL")
                .unwrap_or_else(|_| "wss://api.ramble.my".to_string()),
            conversation_id: std::env::var("CONVERSATION_ID")
                .unwrap_or_else(|_| "tts-server".to_string()),
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
            workers: std::env::var("TTS_WORKERS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(2),
        })
    }

    fn connection_url(&self) -> String {
        let mut url = format!(
            "{}/ws/{}?type=agent&userId={}",
            self.ws_base_url, self.conversation_id, self.user_id
        );
        if let Some(token) = &self.token {
            url.push_str(&format!("&token={token}"));
        }
        url
    }
}

// ---------------------------------------------------------------------------
// Worker pool
//
// Each worker is an independent TTSKoko instance (separate ONNX session).
// Cloning TTSKoko shares its internal mutex, so we need N separate ::new() calls.
//
// acquire() blocks until a worker is free, so requests beyond TTS_WORKERS
// are automatically queued — no messages are dropped.
// ---------------------------------------------------------------------------

struct TtsPool {
    instances: Arc<Mutex<Vec<TTSKoko>>>,
    semaphore: Arc<Semaphore>,
    pub workers: usize,
}

impl TtsPool {
    async fn new(config: &Config) -> Self {
        let mut instances = Vec::with_capacity(config.workers);
        for i in 0..config.workers {
            info!(
                "Loading TTS worker {}/{} …",
                i + 1,
                config.workers
            );
            instances.push(TTSKoko::new(&config.model_path, &config.voices_path).await);
        }
        TtsPool {
            instances: Arc::new(Mutex::new(instances)),
            semaphore: Arc::new(Semaphore::new(config.workers)),
            workers: config.workers,
        }
    }

    /// Check out one worker. Waits if all workers are busy.
    async fn acquire(&self) -> (TTSKoko, OwnedSemaphorePermit) {
        // Acquire permit first — this is where queuing happens.
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .expect("semaphore closed");
        let instance = self
            .instances
            .lock()
            .await
            .pop()
            .expect("semaphore and pool out of sync");
        (instance, permit)
    }

    /// Return a worker to the pool (permit is dropped → next waiter unblocks).
    async fn release(&self, instance: TTSKoko, permit: OwnedSemaphorePermit) {
        self.instances.lock().await.push(instance);
        drop(permit);
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
// Text sanitization (mirrors ramble-web textChunker.sanitizeText)
// ---------------------------------------------------------------------------

static RE_SPACED_HYPHEN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s*-\s*").unwrap());
static RE_QUESTION: Lazy<Regex> = Lazy::new(|| Regex::new(r"\?\s*").unwrap());
static RE_MULTI_NEWLINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());
static RE_MULTI_SPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\S\n]+").unwrap());
// Markdown: **bold**, *italic*, __bold__, _italic_, `code`, ~~strikethrough~~
static RE_MARKDOWN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(\*\*|__|\*|_|`|~~|#{1,6}\s?)").unwrap());

fn sanitize_text(text: &str) -> String {
    let s = text.to_string();

    // Normalize line endings
    let s = s.replace("\r\n", "\n");

    // Strip markdown formatting characters
    let s = RE_MARKDOWN.replace_all(&s, "").to_string();

    // Em-dash / en-dash → comma pause
    let s = s.replace('—', ", ").replace('–', ", ");

    // Spaced hyphen → comma pause (e.g. "word - word")
    let s = RE_SPACED_HYPHEN.replace_all(&s, ", ").to_string();

    // Add a short ellipsis after question marks so Kokoro doesn't rush past them
    let s = RE_QUESTION.replace_all(&s, "? ... ").to_string();

    // Collapse extra blank lines and spaces
    let s = RE_MULTI_NEWLINE.replace_all(&s, "\n\n").to_string();
    let s = RE_MULTI_SPACE.replace_all(&s, " ").to_string();

    s.trim().to_string()
}

// ---------------------------------------------------------------------------
// TTS synthesis → WebSocket send
// ---------------------------------------------------------------------------

async fn synthesize_and_send(
    pool: Arc<TtsPool>,
    tx: mpsc::Sender<String>,
    text: String,
    reply_to_id: String,
    sender_ws_id: Option<String>,
    voice: String,
    language: String,
    speed: f32,
) -> Result<()> {
    let text = sanitize_text(&text);

    info!(
        "Synthesizing ({} chars): {:?}",
        text.len(),
        &text[..text.len().min(80)]
    );

    // Acquire one worker just to split the text, then release it immediately
    // so it's available for parallel chunk synthesis below.
    let chunks: Vec<String> = {
        let (worker, permit) = pool.acquire().await;
        let chunks = worker.split_text_into_speech_chunks(&text, 30);
        pool.release(worker, permit).await;
        chunks
    };

    // Filter empty chunks up front so we know the true total for isFinal.
    let chunks: Vec<String> = chunks
        .into_iter()
        .filter(|c| !c.trim().is_empty())
        .collect();
    let total = chunks.len();

    if total == 0 {
        return Ok(());
    }

    // Spawn one synthesis task per chunk. Each task acquires its own pool
    // worker, so chunks are synthesised in parallel (up to TTS_WORKERS at a
    // time). FuturesOrdered ensures we send them to the client in order even
    // if a later chunk finishes first — eliminating the gap between chunks.
    let mut futures: FuturesOrdered<_> = chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let pool = Arc::clone(&pool);
            let voice = voice.clone();
            let language = language.clone();
            tokio::spawn(async move {
                let (worker, permit) = pool.acquire().await;
                let chunk_text = chunk.clone();
                let voice_ref = voice.clone();
                let lang_ref = language.clone();

                let (returned_worker, samples) = tokio::task::spawn_blocking(move || {
                    let result = worker
                        .tts_raw_audio(&chunk_text, &lang_ref, &voice_ref, speed, None, None, None, None)
                        .map_err(|e| e.to_string());
                    (worker, result)
                })
                .await
                .expect("TTS thread panicked");

                pool.release(returned_worker, permit).await;
                (i, chunk, samples)
            })
        })
        .collect();

    // Stream results in order and send each chunk as it becomes ready.
    let mut send_idx = 0usize;
    while let Some(join_result) = futures.next().await {
        let (i, chunk, samples) = join_result.context("TTS task panicked")?;
        let samples = samples.map_err(|e| anyhow::anyhow!("TTS synthesis: {e}"))?;

        if samples.is_empty() {
            continue;
        }

        let is_final = send_idx == total - 1;
        let duration_ms = (samples.len() as f64 / 24_000.0 * 1000.0) as u64;
        let wav = encode_wav(&samples, 24_000)?;
        let audio_b64 = BASE64.encode(&wav);

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
            send_idx + 1, total, duration_ms, wav.len()
        );

        send_idx += 1;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// WebSocket connection loop (reconnecting)
// ---------------------------------------------------------------------------

/// Dedup cache: message id → time received. Shared across reconnects.
type SeenMessages = Arc<Mutex<HashMap<String, Instant>>>;

const DEDUP_WINDOW: Duration = Duration::from_secs(10);

async fn run(config: Arc<Config>, pool: Arc<TtsPool>) {
    let seen: SeenMessages = Arc::new(Mutex::new(HashMap::new()));
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
                backoff = Duration::from_secs(1);

                let (mut sink, mut stream) = ws.split();
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
                                    info!(
                                        "Registered as agent (tts-server)  wsId={ws_id}  workers={}",
                                        pool.workers
                                    );
                                }

                                "ping" => {
                                    let pong = WsMessage::new("pong", None, serde_json::json!({}));
                                    let _ = tx.send(serde_json::to_string(&pong).unwrap()).await;
                                }

                                "tts_request" => {
                                    // Dedup: ignore the same message id within DEDUP_WINDOW.
                                    {
                                        let mut cache = seen.lock().await;
                                        let now = Instant::now();
                                        // Evict stale entries to keep the map bounded.
                                        cache.retain(|_, t| now.duration_since(*t) < DEDUP_WINDOW);
                                        if cache.contains_key(&msg.id) {
                                            warn!("Duplicate tts_request {} — ignoring", msg.id);
                                            continue;
                                        }
                                        cache.insert(msg.id.clone(), now);
                                    }

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

                                    let (pool, tx) = (Arc::clone(&pool), tx.clone());
                                    // Use voice from payload if provided, else fall back to config default.
                                    let v = msg
                                        .payload
                                        .get("voice")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(&config.voice)
                                        .to_string();
                                    let (l, sp) = (
                                        config.language.clone(),
                                        config.speed,
                                    );
                                    tokio::spawn(async move {
                                        if let Err(e) = synthesize_and_send(
                                            pool, tx, text_to_speak, reply_to, sender_id, v, l, sp,
                                        )
                                        .await
                                        {
                                            error!("TTS task error: {e}");
                                        }
                                    });
                                }

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

                                    let (pool, tx) = (Arc::clone(&pool), tx.clone());
                                    let (v, l, sp) = (
                                        config.voice.clone(),
                                        config.language.clone(),
                                        config.speed,
                                    );
                                    tokio::spawn(async move {
                                        if let Err(e) = synthesize_and_send(
                                            pool, tx, text_to_speak, reply_to, None, v, l, sp,
                                        )
                                        .await
                                        {
                                            error!("TTS task error: {e}");
                                        }
                                    });
                                }

                                _ => {}
                            }
                        }

                        Ok(Message::Close(_)) => {
                            info!("Server closed connection");
                            break;
                        }

                        Ok(Message::Ping(_)) => {}

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

    info!(
        "Initialising {} TTS worker(s) from {}",
        config.workers, config.model_path
    );
    let pool = Arc::new(TtsPool::new(&config).await);
    info!(
        "Pool ready  workers={}  voice={}  lang={}",
        pool.workers, config.voice, config.language
    );

    run(config, pool).await;

    Ok(())
}
