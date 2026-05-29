# BaleVPN — Architecture & Build Guide

This document covers the **repository layout, the runtime architecture, and how to build** each component from source. End-user setup lives in the rest of `docs/` (Android, Rust per-OS). For protocol-level internals (wire format, codec choices, RPC framing) see [`CLAUDE.md`](../CLAUDE.md).

---

## Repository layout

```
BaleVPN/
├── reverse_engineering/        # Bale web-app crawler + protobuf extractor
│   ├── download.py             # Downloads Bale's webpack bundles
│   └── extract_proto.py        # Emits .proto files into bale-signaling-rust/
│
├── lk-signaling-rust/          # Generic signaling TRAIT crate
│   └── lk-signaling/           # ─ no Bale knowledge; just the abstract surface
│                                 (PeerRef, Signaling, TokenStore, IncomingHandler)
│
├── bale-signaling-rust/        # Bale IMPLEMENTATION of lk-signaling
│   └── bale-signaling/
│       ├── proto/              # Extracted .proto files (regenerated)
│       └── src/                # WS client + auth + contacts + dispatch
│
├── lktunnel-rust/              # WebRTC (webrtc-rs) transport + QUIC + NAT + TUN core
│   ├── lktunnel/               # ─ the library (rtc engine, carrier, NAT, TUN)
│   └── cli/                    # ─ standalone CLI (dev/test harness)
│
├── bale-vpn-rust/              # The unified Rust binary (client / server / GUI)
│   └── bale-vpn/
│       ├── assets/index.html   # Embedded HTML/JS UI (single file)
│       └── src/                # main.rs, daemon, ui (HTTP), ui_native (wry)
│
├── bale-vpn-android/           # Android app
│   ├── androidApp/             # Kotlin UI + foreground services + JNI loaders
│   └── rust/                   # Cargo workspace for the JNI shim
│       ├── jni-shared/         # Generic handle registry + runtime + JNI helpers
│       ├── lktunnel-jni/       # Merged JNI shim — wraps BOTH lktunnel-rust
│       │                       # AND bale-signaling-rust (plus the manager
│       │                       # layer); produces `liblktunnel_jni.so`
│       └── jniLibs/<abi>/      # GITIGNORED; cargo-ndk output
│
├── docs/                       # User-facing per-platform guides + screenshots
│   ├── android-en.md / android-fa.md
│   ├── rust-en.md    / rust-fa.md
│   └── screens/                # Screenshot library
│
├── .github/workflows/          # CI: rust.yml, android.yml, bale-vpn-rust.yml
├── README.md                   # Project overview + privacy notes + links
├── CLAUDE.md                   # Protocol internals + dev notes
└── ARCHITECTURE.md             # This file
```

### Crate dependency graph

Three "shared core" pure-Rust libraries do the actual work; the desktop binary and the Android JNI shim are both thin consumers of them.

```
                ┌────────────────────────────────────────────────────────┐
                │               Shared core  (pure Rust)                 │
                │                                                        │
                │                    lk-signaling                        │
                │           (Signaling + TunnelHooks traits)             │
                │             ▲                       ▲                  │
                │  implements │                       │ consumes via     │
                │             │                       │  Arc<dyn Signaling>
                │   ┌─────────┴───────┐    ┌──────────┴─────────────┐    │
                │   │  bale-signaling │    │       lktunnel         │    │
                │   │  WS + auth +    │    │  managers (Client /    │    │
                │   │  contacts +     │    │  Server) + tunnels     │    │
                │   │  events         │    │  (webrtc-rs) + QUIC    │    │
                │   │                 │    │  carrier + NAT + TUN   │    │
                │   └─────────────────┘    └────────────────────────┘    │
                └────────────────────────────────────────────────────────┘
                          ▲                                ▲
                          │ uses all three                 │ uses all three
                          │                                │
        ┌─────────────────┴───────────┐    ┌───────────────┴────────────┐
        │       bale-vpn-rust         │    │       lktunnel-jni         │
        │  single binary, CLI + GUI   │    │   merged Android JNI shim  │
        │                             │    │   + jni-shared dep         │
        └─────────────────────────────┘    └───────────────▲────────────┘
                                                           │ System.loadLibrary
                                                           │
                                           ┌───────────────┴────────────┐
                                           │  bale-vpn-android (Kotlin) │
                                           │  ─ MainActivity, services  │
                                           │  ─ JNI Kotlin wrappers     │
                                           └────────────────────────────┘
```

Edge details:

- `bale-vpn-rust` depends on **all three** core crates: `lk-signaling` + `bale-signaling` + `lktunnel`.
- `bale-signaling` depends only on `lk-signaling`. It has **no transport knowledge** — the lifecycle coupling is one-way: the `lktunnel` managers subscribe to the signaling impl's `TunnelHooks` (foreground, teardown, CallEnded fan-out) and push a single `manager_active` boolean back to gate the WS rule engine. The transport is pure Rust (`webrtc-rs` + crates.io `livekit-api`/`livekit-protocol`); no vendored crates, no `[patch.crates-io]`.
- `lktunnel` depends on `lk-signaling`. The `manager.rs` module wraps an `Arc<S: Signaling>` to construct `ClientTunnelManager` / `ServerTunnelManager` and the process-singleton `ActivationDriver`.
- The Android JNI surface is a **single merged crate**, `lktunnel-jni`, depending on `bale-signaling` + `lktunnel` + `lk-signaling` + `jni-shared`. It produces one `.so` covering `Java_ai_bale_proxy_{LkTunnelNative, LkManagerNative, NativeJni, bale_BaleSignalingNative, bale_BaleAuthNative}_*`. The merge is deliberate — a per-crate split would static-link the shared transport (webrtc-rs + quinn + tokio) twice for no benefit.
- `jni-shared` is consumed only by `lktunnel-jni`; it carries the generic `HandleRegistry<T>`, the shared tokio runtime, the JavaVM cache, and the async-JNI continuation bridge (`spawn_with_continuation` — Kotlin `suspend` functions back onto Rust futures without blocking an IO thread per call).
- The Bale protocol implementation has **one source of truth** — the `bale-signaling` crate — and both targets consume it via the same Rust API. No protocol parsing exists in Kotlin: every WS frame, every RPC, every contact entry is parsed once in Rust and surfaced through the JNI shim.

---

## Runtime architecture

### Signaling vs transport

Two independent channels per session:

1. **Signaling** — Bale's WebSocket at `wss://next-ws.bale.ai/ws/`. Carries the call setup RPCs (`StartCall`, `AcceptCall`, `DiscardCall`), presence (`SetOnline`), contact list, and incoming-call push notifications. Lives in `bale-signaling-rust/`.

2. **Transport** — a LiveKit-SFU room joined per active call, on the pure-Rust **`webrtc-rs`** stack (no libwebrtc). Lives in `lktunnel-rust/` (`rtc.rs` = the engine, `rtp.rs` = framing). The tunnel bytes are written **directly** as the RTP payload of one published Opus "audio" track (`TrackLocalStaticSample::write_sample`) — there's no codec and no data channel; the SFU relays opaque Opus RTP and the far end reads the bytes back off `read_rtp`. RTP-over-SRTP looks like an ordinary voice call and survives DPI that fingerprints SCTP data channels.

Each call gets its own `rtc::Engine` (LiveKit's two-PeerConnection model — publisher + subscriber — over the reused `livekit-api` signal client, impersonating the JS SDK via `sdk=js`), plus a per-tunnel mio reactor (`dispatcher.rs`) for the QUIC + NAT + TUN side. No global `PeerConnectionFactory` — webrtc-rs builds each PC from an `APIBuilder` (`make_api()`).

### WS lifecycle policy (input-driven, in the library)

The WS lifetime is owned by `bale-signaling-rust`. Apps don't call `connect()` / `disconnect()` to drive it; they push inputs into the rule engine and observe `is_connected()`. The library evaluates a fixed three-input rule:

**Rule** (in `ws.rs::desired_up`):

```
want_up = token.is_some()
       && !user_disconnect
       && manager_active
```

**Inputs:**

| Input             | Set by                                                       | Default |
|-------------------|--------------------------------------------------------------|---------|
| `token`           | `set_token(Some(...))` (auth flow)                           | `None`  |
| `user_disconnect` | `set_user_disconnect(true)` (UI Disconnect button)           | `false` |
| `manager_active`  | `set_manager_active(bool)` from `lktunnel::manager::ActivationDriver` | `true` |

`manager_active` is the **only** mode/foreground-aware gate. The complexity (client vs server semantics, foreground/background, active-session count) lives **one layer up**, in `lktunnel::manager::ActivationDriver`, which subscribes to the signaling impl's `TunnelHooks::subscribe_foreground` and computes:

```
manager_active = match mode {
    Server => true,                    // always-on; the server can't accept
                                       // calls while the WS is down
    Client => foreground && active_session_count <= 0,
}
```

The driver pushes `manager_active` via `TunnelHooks::activate()` / `deactivate()` on every input change. This keeps `bale-signaling-rust` free of any transport / mode knowledge — its rule is just three booleans.

**Process-singleton driver.** The driver survives client↔server mode swaps. The Rust binary constructs it once during daemon startup and threads `Arc<ActivationDriver<BaleSignaling>>` into each `ClientTunnelManager::new` / `ServerTunnelManager::new`. On Android, `lktunnel-jni` lazily initialises a process-static driver on first manager construction (`process_driver()` in the JNI manager module); Kotlin never sees it — there is no `ActivationDriver` class on the Kotlin side. Either way the driver outlives any individual manager.

**Input sources by platform:**

| Input                      | Rust binary                                          | Android Kotlin                                       |
|----------------------------|------------------------------------------------------|------------------------------------------------------|
| `token`                    | auth flow → `BaleSignaling::auto_load_token` from disk | auth flow → `SharedPreferences` (via JNI bridge)   |
| `user_disconnect`          | `POST /disconnect` HTTP endpoint                     | `MainActivity.btnWs` Disconnect button               |
| mode (driver-internal)     | `ServerTunnelManager::new` flips to Server; default Client | `BaleConnection.setMode(Mode.SERVER/CLIENT)`    |
| foreground (driver-internal) | always `true` (headless process)                   | `BaleApp` → `ProcessLifecycleOwner` observer         |
| active sessions (driver-internal) | per-call `spawn_client_watcher` (Connected → +1, Disconnected → −1) | same                       |

### Manager layer (`lktunnel::manager`)

The thin per-platform adapter between the Bale signaling layer and the LK transport's per-session state. Two flavours, both generic over `S: Signaling`:

- **`ClientTunnelManager`** — one outgoing call at a time. `place_call(peer)` resolves Bale `StartCall`, builds an `LkTunnel`, and spawns a per-call watcher that drives the `ActivationDriver`'s session counter and surfaces `SessionEvent::{Connected, Disconnected, Failed}` on a per-manager fan-out `EventsSink`. Re-calling `place_call` replaces the prior call: the old tunnel is `disconnect()`ed, the new entry takes the `current` slot, and the old watcher's eventual Disconnected/Failed is **suppressed** if its tunnel no longer matches `current` (`Arc::ptr_eq`) — otherwise the new dial's collector would wake to a stale "Failed for $peer" and abort.

- **`ServerTunnelManager`** — many concurrent incoming calls. Wraps the consumer's `IncomingHandler` with an internal `decide()` that registers each in-flight admission decision in a per-peer `pending` map. Accepted sessions surface as `SessionEvent::Connected { peer_id, tunnel }`; the per-session watcher emits `Disconnected` on LK teardown.

Both managers subscribe internally to `TunnelHooks::subscribe_call_ended` (a multi-subscriber fan-out separate from the single-consumer `Signaling::events` stream) and apply the **"LK is the sole authority once joined"** rule centrally:

- **Pre-LK** Bale `CallEnded` for the dialed / pending peer → tear the in-flight tunnel down (client) or cancel the pending `decide()` (server). The consumer sees `SessionEvent::Failed`.
- **Post-LK** Bale `CallEnded` for an active session → **ignored**. LK's own engine `Disconnected` event is the only authoritative end-of-session signal. This prevents a transient Bale WS hiccup (cardinality re-subscribe, brief reconnect) from dropping a live tunnel.

Apps consume only `SessionEvent` from the manager — they no longer subscribe to `SignalingEvent::CallEnded` directly. The Rust daemon (`run_client` / `run_server`) and the Android Kotlin services (`BaleVpnService` / `BaleServerService`) both stopped wiring their own WS `CallEnded` watchers; the invariant lives in exactly one place.

### Server-side data flow

```
   Bale WS  ── callReceived ──▶  AdmissionHandler.decide
                                    │
                                    ▼
                              AcceptCall RPC ─▶ LK creds
                                    │
                                    ▼
                              LkTunnel::connect_server
                                    │
                                    ▼
                              start_server
                                    │
                ┌───────────────────┴────────────────────┐
                │                                        │
                ▼                                        ▼
        Kernel TUN attach                       Userspace NAT
        (10.8.K.0/24 per peer)                  (per-flow state)
                │                                        │
                ▼                                        ▼
        iptables/pf MASQUERADE                   host TCP/UDP sockets
                │                                        │
                └───────────────┬────────────────────────┘
                                ▼
                          ── internet ──
```

In kernel mode each accepted call gets its own `bale<K>` (Linux) or `utunN` (macOS) device at `10.8.K.0/24`. The kernel handles forwarding + NAT. In userspace mode every flow becomes a per-flow Rust TCP/UDP state machine (`lktunnel/src/nat/*`) with full TCP semantics (SACK, RACK, TLP, PRR, window scaling).

### Client-side data flow

The two ingress sources are independent — either or both can be active. They use **different frame types** inside the same RTP media carrier; the server's `LkTunnel` demuxes on the first byte.

```
   Local apps                            Host networking stack
   ──────────                            ─────────────────────
       │                                          │
       ▼                                          ▼
   SOCKS5 listener at 127.0.0.1:1080       System TUN at 10.8.0.2/24
   (TCP connect / UDP associate)           (raw IP packets, needs
                                            `--client-tun` + route setup)
       │                                          │
       │ each conn becomes a QUIC stream          │ raw IP packet bytes
       ▼                                          ▼
   QUIC client  ──────► datagrams           prepended with FRAME_TYPE_IP
       │                                          │
       │ datagrams prepended with FRAME_TYPE_QUIC │
       └───────────────────┬──────────────────────┘
                           │
                           ▼
                  LkTunnel send pipeline
                  (sender task → RtpSender)
                           │
                           ▼
           webrtc-rs RTP carrier — frames written
           directly as one published Opus track's
           payload (write_sample); SFU relays opaque
           Opus, far end reads them off read_rtp
                           │
                           ▼
                 server-side LkTunnel
                 (demux on frame-type byte)
                           │
                ┌──────────┴──────────┐
                ▼                     ▼
        QUIC stream acceptor    Server NAT  /  kernel TUN
        (host TCP connect)         (kernel forwarding)
                │                     │
                └──────────┬──────────┘
                           ▼
                      ── internet ──
```

The client opens **one** LiveKit call to the configured server peer. SOCKS5 connections become QUIC streams over a single QUIC connection riding the RTP carrier; TUN-routed IP packets ride the **same** carrier but go in as raw `FRAME_TYPE_IP` payloads (no QUIC — the guest's own TCP handles loss). The server-side `LkTunnel` peeks the first byte to route each inbound frame to the QUIC acceptor or the NAT/TUN bridge respectively.

### Android JNI bridge

The Kotlin UI calls into Rust through a **single merged JNI shim**:

- **`lktunnel-jni`** (`liblktunnel_jni.so`) — covers every JNI symbol the app needs: `Java_ai_bale_proxy_{LkTunnelNative, LkManagerNative, NativeJni, bale_BaleSignalingNative, bale_BaleAuthNative}_*`. Depends on `lktunnel-rust`, `bale-signaling-rust`, `lk-signaling-rust`, and `jni-shared`.
- **`jni-shared`** (consumed as a Rust dep only — no `.so` of its own) — generic `HandleRegistry<T>`, shared tokio runtime, JavaVM cache, jstr helpers, **`spawn_with_continuation`** (the async-JNI bridge — Kotlin `suspend` functions back onto Rust futures without blocking an IO thread per call).

The Kotlin side mirrors the merged shim with focused `object` declarations under `ai.bale.proxy`: `LkTunnelNative`, `LkManagerNative`, `BaleSignalingNative`, `BaleAuthNative`, `NativeJni`. All of them `System.loadLibrary("lktunnel_jni")` in their `init {}` blocks — whichever class the app touches first triggers `JNI_OnLoad`.

JNI handles are opaque `jlong` ids that index into the registry, so concurrent Kotlin calls aren't serialised by an instance monitor — they each acquire their own `Arc<T>` clone.

---

## How to build

### Common prerequisites

| Tool | Required for | Notes |
|---|---|---|
| Rust stable | every build | `rustup default stable` |
| Python 3 | regenerating protos | `reverse_engineering/extract_proto.py`; not needed if you don't touch the protocol |
| OpenJDK 17 + Android SDK + NDK | Android build | NDK pinned to `28.0.13004108` in `bale-vpn-android/androidApp/build.gradle.kts` |
| `cargo-ndk` | Android Rust build | `cargo install cargo-ndk --locked`; auto-installed by the Android workflow |
| Linux: GTK / GLib / webkit2gtk dev headers | GUI build on Linux | `libgtk-3-dev libwebkit2gtk-4.1-dev libsoup-3.0-dev libssl-dev pkg-config` (the GUI/`wry` deps; the transport is pure Rust, no audio/libwebrtc headers needed) |

### Step 1 — Rust desktop binary (`bale-vpn-rust`)

The transport is pure Rust now (`webrtc-rs` + crates.io `livekit-api`/`livekit-protocol`) — there's no vendored crate tree and no patch step, so a plain `cargo build` works. The Makefile targets are just convenience wrappers:

```bash
make build              # = cargo build --release          (GUI: tao + wry)
make build-headless     # = cargo build --release --no-default-features
# or directly:
cd bale-vpn-rust && cargo build --release
./bale-vpn-rust/target/release/bale-vpn
```

Cargo features:

| Feature | Default | Pulls in | Effect |
|---|---|---|---|
| `gui` | yes | `tao`, `wry`, `muda` | Embedded webview opens at startup; `--headless` skips the window |

### Step 3 — Android APK + JNI `.so`

Via Make:

```bash
make android              # = ./gradlew :androidApp:assembleRelease (needs keystore env vars)
make android-debug        # = ./gradlew :androidApp:assembleDebug
make install-android      # adb-installs the release APK on a connected device
make install-android-debug
```

Or directly:

```bash
cd bale-vpn-android
JAVA_HOME=<jdk17> ./gradlew :androidApp:assembleDebug      # debug APK
JAVA_HOME=<jdk17> ./gradlew :androidApp:assembleRelease    # release APK
```

`assembleDebug` runs the custom `cargoBuild` Gradle task first, which invokes `cargo-ndk` to cross-compile the merged JNI shim (`lktunnel-jni` + its `jni-shared` dep) for `arm64-v8a`, `armeabi-v7a`, and `x86_64`. Output lands in `bale-vpn-android/rust/jniLibs/<abi>/liblktunnel_jni.so`; `androidApp` picks it up via `jniLibs.srcDir(file("../rust/jniLibs"))`. Which ABIs get *packaged* is set per build type via `abiFilters`: **release = arm64-v8a + armeabi-v7a** (no x86_64 — emulators only), debug = arm64-v8a + x86_64.

Release signing reads:

```bash
ANDROID_KEYSTORE_PATH      # path to .jks
ANDROID_KEYSTORE_PASSWORD  # store pw
ANDROID_KEY_ALIAS          # key alias
ANDROID_KEY_PASSWORD       # key pw
```

If any is missing or the keystore file doesn't exist, `assembleRelease` falls back to the debug signing config — useful for local testing.

### Step 4 — Regenerate the Bale proto definitions (optional)

Only needed when Bale ships a new web bundle:

```bash
python3 reverse_engineering/download.py     # downloads JS bundles; needs a valid access_token in the script
python3 reverse_engineering/extract_proto.py # writes .proto into bale-signaling-rust/bale-signaling/proto/
```

The Rust signaling crate doesn't actually compile these `.proto` files (the wire encoding is hand-rolled in `proto.rs` for the field shapes Bale uses). They're checked in as documentation + a starting point if you ever want to switch to `prost` / `tonic`.

### Tests

```bash
cd lktunnel-rust       && cargo test --workspace --release
cd lk-signaling-rust   && cargo test --workspace --release
cd bale-signaling-rust && cargo test --workspace --release
```

Most coverage lives in `bale-signaling-rust` (proto codec round-trip, gRPC-web envelope, WS frame dispatch, RPC pending machinery, contact list parsing) and `lktunnel-rust` (NAT state machines, TCP option negotiation, dispatcher reactor). `bale-vpn-rust` itself has minimal tests — it's mostly axum handlers + signaling glue.

---

## CI / release

| Workflow | File | Trigger | Output |
|---|---|---|---|
| Rust tests | `.github/workflows/rust.yml` | push + PR | cargo test on Linux + macOS + Windows; clippy |
| Android APK | `.github/workflows/android.yml` | push + PR + `v*` tag | APK artifact; signed + attached to GitHub Release on tag |
| Desktop binaries | `.github/workflows/bale-vpn-rust.yml` | push + PR + `v*` tag | Per-OS `bale-vpn-{gui,headless}-{linux,macos,windows}-<arch>` binaries; attached to GitHub Release on tag |

Tags follow `v<semver>` (e.g. `v0.1.0`). One tag pushes both the APK and the desktop binaries together.

---

## Development conventions

- **Single source of truth for protocol shape** — `bale-signaling-rust/`. Anything that talks to Bale (Android, desktop) goes through this crate. No duplicate WS parsing in Kotlin.
- **Don't add features without a request.** The codebase grew organically; the architecture review (`CLAUDE.md` + this file) is meant to keep it from growing further. New flags / endpoints / modes need a user-visible justification.
- **One file per crate's worth of new state.** Don't sprinkle handler-specific state across `daemon.rs` / `server.rs` / `client.rs` — the existing split (per-mode files) is load-bearing for the run-loop reasoning.
- **Comments explain WHY, not WHAT.** Identifiers carry the WHAT. Comments record hidden constraints, prior incidents, and surprising decisions. See `CLAUDE.md` for the style.
