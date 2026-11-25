# eternAI

A persistent, dual-model AI companion. One conversation, two devices:

- **Desktop (high power):** a large local model (llama.cpp `llama-server`) with a
  full 200k context, running behind the **gateway** — a Rust/axum service that
  owns the canonical conversation store, the Journal, memory, tool execution,
  TLS, and phone pairing.
- **Phone (edge):** a small model running on-device (Android) with an 8k
  window. When the gateway is reachable, the phone's prompts are served by the
  desktop model over the network; when it isn't, the edge model takes over and
  everything is queued for sync. The edge context is kept within the latest
  ~6k tokens of the desktop conversation so the handoff is seamless.

Both models share one harness (`etern-core`) so the sync protocol, token
budgets, Journal rendering, and tool contract cannot diverge between devices.

## Repository layout

```
crates/
  etern-core     Shared harness: conversation store, token budgets, Journal,
                 sync protocol, sandboxed tool runtime (read/write/list/grep/
                 web_search/fetch_page/remember)
  etern-gateway  Desktop gateway: TLS server, engine proxy, pairing, sync,
                 journal, tool execution, SearXNG-backed web search, page fetch
  etern-ffi      UniFFI bindings that run the harness + edge tools on Android
apps/
  desktop        Tauri (React/TS) desktop app: chat UI, tool loop, pairing QR
  android        Android app (Kotlin + native llama.cpp): edge model + harness
PLAN.md          The full design document (architecture, budgets, decisions)
```

## Prerequisites

- Rust (stable) and Node.js (for the desktop frontend)
- llama.cpp `llama-server` serving the desktop model on `http://localhost:8080`
- (Optional) SearXNG for `web_search`, e.g. at `http://127.0.0.1:5678`
- (Optional) Tailscale — the gateway auto-advertises its Tailscale IP so the
  phone can pair over the tailnet when off the home network
- Android: JDK 17+, Android SDK, NDK (for the native llama.cpp build)

## Build & run (desktop)

```bat
:: Gateway (from the directory where your Journal/memory/inbox live)
start-gateway.bat

:: Desktop app (dev mode)
run-desktop.bat
```

Or manually:

```bat
cargo build -p etern-gateway
etern-gateway.exe --tls-listen=8443 --searxng=http://127.0.0.1:5678

cd apps\desktop
npm install
npm run tauri dev
```

The desktop app shows a pairing QR code (choose Local Wi-Fi or Tailscale);
scan it once with the Android app to pair.

## Tests

```bat
cargo test
```

## Notes

- The gateway generates its own self-signed TLS certificate on first start
  (`apps/desktop/tls/`); the phone trusts it via the pairing pin.
- Runtime data (conversation store `etern.db`, `Journal/`, `memory/`,
  `inbox/`, TLS keys, logs) is intentionally **not** committed — see
  `.gitignore`.
