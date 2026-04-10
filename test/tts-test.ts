/**
 * TTS end-to-end test
 *
 * Connects to the ramble WebSocket as a "runtime" client, sends a tts_request
 * to any connected tts-server agent, then receives tts_audio chunks back and
 * plays each one through macOS afplay (sequential, streaming).
 *
 * Usage:
 *   tsx tts-test.ts
 *   tsx tts-test.ts "Your custom text here"
 */

import WebSocket from "ws";
import { spawn } from "child_process";
import { writeFileSync, mkdirSync, rmSync } from "fs";
import { tmpdir } from "os";
import { join } from "path";
import crypto from "crypto";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

const WS_BASE = process.env.WS_URL ?? "wss://api.ramble.my";
const CONVERSATION_ID = process.env.CONVERSATION_ID ?? "test";
const USER_ID = "test-client-" + crypto.randomBytes(3).toString("hex");
const TEXT_TO_SPEAK =
  process.argv[2] ??
  "Hello! This is a streaming text-to-speech test using Kokoro running in Rust. " +
    "Each sentence is synthesised and sent back as a separate audio chunk. " +
    "Can you hear all three parts arriving one by one?";

const WS_URL = `${WS_BASE}/ws/${CONVERSATION_ID}?type=runtime&userId=${USER_ID}`;

// ---------------------------------------------------------------------------
// Types (mirrors the server WsMessage envelope)
// ---------------------------------------------------------------------------

interface WsMessage {
  id: string;
  type: string;
  from?: { type?: string; id?: string };
  to?: { type?: string; id?: string };
  payload: Record<string, unknown>;
}

interface TtsAudioPayload {
  inReplyToMessageId: string;
  chunkIndex: number;
  isFinal: boolean;
  text: string;
  audioData: string; // base64 WAV
  sampleRate: number;
  durationMs: number;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

let msgCounter = 0;
function makeMessage(type: string, to: WsMessage["to"], payload: object): WsMessage {
  return {
    id: crypto.randomUUID(),
    type,
    to,
    payload: payload as Record<string, unknown>,
  };
}

/** Play a WAV buffer through afplay and resolve when playback finishes. */
function playWav(wavBuffer: Buffer): Promise<void> {
  return new Promise((resolve, reject) => {
    // Write to a unique temp file
    const dir = join(tmpdir(), "tts-test-chunks");
    mkdirSync(dir, { recursive: true });
    const file = join(dir, `chunk-${msgCounter++}-${Date.now()}.wav`);
    writeFileSync(file, wavBuffer);

    const proc = spawn("afplay", [file], { stdio: "ignore" });
    proc.on("close", (code) => {
      try { rmSync(file); } catch {}
      if (code === 0) resolve();
      else reject(new Error(`afplay exited with code ${code}`));
    });
    proc.on("error", reject);
  });
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
  console.log(`\n[tts-test] Connecting to ${WS_URL}\n`);

  const ws = new WebSocket(WS_URL);

  // Queue to play chunks sequentially without gaps
  let playQueue = Promise.resolve();
  let chunksReceived = 0;
  let startTime: number | null = null;
  let agentReady = false;

  const timeout = setTimeout(() => {
    console.error(
      "\n[tts-test] ⏱  Timed out after 90 s — is the tts-server running?\n"
    );
    ws.close();
    process.exit(1);
  }, 90_000);

  function sendTtsRequest() {
    if (agentReady) return; // already sent
    agentReady = true;
    const req = makeMessage(
      "tts_request",
      { type: "tts-server" },
      { text: TEXT_TO_SPEAK }
    );
    ws.send(JSON.stringify(req));
    startTime = Date.now();
    console.log(
      `[tts-test] 📤 Sent tts_request:\n    "${TEXT_TO_SPEAK}"\n`
    );
  }

  ws.on("open", () => {
    console.log("[tts-test] ✅ WebSocket open");
  });

  ws.on("message", (raw: Buffer) => {
    let msg: WsMessage;
    try {
      msg = JSON.parse(raw.toString());
    } catch {
      console.warn("[tts-test] non-JSON message, ignoring");
      return;
    }

    switch (msg.type) {
      case "connected": {
        const { wsId, connectedCount } = msg.payload as {
          wsId: string;
          connectedCount: number;
        };
        console.log(
          `[tts-test] 🔗 Connected  wsId=${wsId}  peers=${connectedCount}`
        );
        // If the agent is already in the room, send immediately.
        // Otherwise wait for a "presence" event showing an agent joined.
        if (connectedCount > 1) {
          console.log("[tts-test] 🤖 Agent already present — sending request");
          sendTtsRequest();
        } else {
          console.log("[tts-test] ⏳ Waiting for tts-server agent to join…");
        }
        break;
      }

      case "tts_audio": {
        const p = msg.payload as unknown as TtsAudioPayload;
        chunksReceived++;

        const elapsed = startTime ? Date.now() - startTime : 0;
        if (chunksReceived === 1) {
          console.log(`[tts-test] ⚡ Time to first audio: ${elapsed} ms`);
        }

        console.log(
          `[tts-test] 🎵 Chunk ${p.chunkIndex + 1}  isFinal=${p.isFinal}  ` +
            `${p.durationMs} ms  "${p.text.slice(0, 60)}"`
        );

        // Decode and enqueue for sequential playback
        const wavBuffer = Buffer.from(p.audioData, "base64");
        playQueue = playQueue.then(() =>
          playWav(wavBuffer).catch((e) =>
            console.error("[tts-test] playback error:", e)
          )
        );

        if (p.isFinal) {
          playQueue.then(() => {
            const total = Date.now() - (startTime ?? 0);
            console.log(
              `\n[tts-test] ✅ Done — ${chunksReceived} chunks, total ${total} ms\n`
            );
            clearTimeout(timeout);
            ws.close();
          });
        }
        break;
      }

      case "presence": {
        const { userId, clientType, event } = msg.payload as {
          userId: string;
          clientType: string;
          event: string;
        };
        console.log(
          `[tts-test] 👥 Presence: ${clientType} ${userId} ${event}`
        );
        // Fire the request as soon as we know an agent has joined.
        if (clientType === "tts-server" && event === "joined") {
          sendTtsRequest();
        }
        break;
      }

      case "ping": {
        const pong = makeMessage("pong", undefined, {});
        ws.send(JSON.stringify(pong));
        break;
      }
    }
  });

  ws.on("error", (err) => {
    console.error("[tts-test] ❌ WebSocket error:", err.message);
    clearTimeout(timeout);
    process.exit(1);
  });

  ws.on("close", (code, reason) => {
    if (code !== 1000 && code !== 1005) {
      console.log(`[tts-test] Connection closed  code=${code}  reason=${reason}`);
    }
  });
}

main().catch((err) => {
  console.error("[tts-test] Fatal:", err);
  process.exit(1);
});
