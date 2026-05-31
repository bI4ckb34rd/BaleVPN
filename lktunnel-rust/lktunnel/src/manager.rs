//! Tunnel managers — generic glue between [`lk_signaling::Signaling`]
//! and [`crate::LkTunnel`].
//!
//! Three public types:
//!
//! * [`ActivationDriver`] — long-lived, owned by the consumer
//!   (`BaleConnection` on Android, the daemon top-level on Rust).
//!   Subscribes to foreground at construction; tracks live
//!   session count and current mode (client / server). Computes
//!   the activate/deactivate intent and pushes it to the
//!   signaling impl on transitions. Survives mode swaps so the
//!   gate doesn't flip across `Client` ↔ `Server` manager
//!   handoffs.
//!
//! * [`ClientTunnelManager`] — one outgoing call at a time.
//!   Configures the driver for client mode on construction.
//!   `place_call` and the per-call watcher bump the session
//!   count; the driver gates on `foreground && no-session`.
//!   No explicit hang-up: drop the `LkTunnel` (or call
//!   `tunnel.disconnect()`) and the watcher cleans up.
//!
//! * [`ServerTunnelManager`] — many concurrent incoming calls.
//!   Configures the driver for server mode on construction
//!   (always-active, foreground irrelevant). The per-session
//!   watcher doesn't bump the session count — server semantics
//!   don't depend on it.

use crate::LkTunnel;
use lk_signaling::{
    CallDecision, EndReason, EventsSink, IncomingHandler, PeerId, PlaceCallError, Signaling,
    SignalingEvent,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Weak;
use tokio::sync::{mpsc, oneshot};

// ─── Public surface ────────────────────────────────────────────────────

/// Per-peer tunnel lifecycle. Emitted by both managers.
///
/// Three terminal states a session can land in:
///   - `Connected`     — handshake succeeded, tunnel ready.
///   - `Disconnected`  — was Connected, now torn down.
///   - `Failed`        — never reached Connected (handshake
///                        failure, peer never joined, etc.).
///
/// `Clone` so the [`EventsSink`] fan-out can hand each subscriber its own
/// copy (`peer_id` + an `Arc<LkTunnel>` handle — cheap).
#[derive(Clone)]
pub enum SessionEvent {
    Connected    { peer_id: PeerId, tunnel: Arc<LkTunnel> },
    Disconnected { peer_id: PeerId },
    Failed       { peer_id: PeerId },
}

/// Which mode the [`ActivationDriver`] is currently in. The
/// manager picks the mode at construction; the driver gates the
/// control channel accordingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Outgoing-call semantics: gate the control channel on
    /// `foreground && no-active-session`. The push channel
    /// pauses while a call is up.
    Client,
    /// Incoming-call semantics: keep the control channel up
    /// unconditionally. Required to receive `callReceived`
    /// pushes; foreground state is irrelevant.
    Server,
}

// ─── Activation driver ────────────────────────────────────────────────

/// Long-lived owner of the WS-activation gate. Survives mode
/// swaps; the managers come and go around it.
///
/// State the driver owns:
///   - mode       — `Client` or `Server`; selected by the
///                  manager that's currently using the driver.
///                  Switched via [`Self::set_mode`].
///   - foreground — subscribed to `signaling.subscribe_foreground`
///                  at construction; tracked atomically.
///   - active     — session count for client mode. Server mode
///                  ignores this. Clamped to `>= 0` to absorb
///                  out-of-order Disconnected events from
///                  tunnels left over after a mode swap.
///   - last_push  — dedupes consecutive identical intents so
///                  no-op pushes never churn the WS run loop.
pub struct ActivationDriver<S: Signaling + ?Sized> {
    signaling:   Weak<S>,
    server_mode: AtomicBool,    // backing for the public `Mode` enum
    foreground:  AtomicBool,
    active:      AtomicI64,
    last_push:   AtomicBool,
    /// Serialises [`Self::reconcile`] so the `want` value
    /// computed inside the critical section is the one that
    /// gets swapped + pushed. Without this, concurrent
    /// `bump_session` callers can race and leave `last_push`
    /// out of sync with the actual desired state — see the
    /// architecture review notes.
    reconcile_lock: std::sync::Mutex<()>,
}

impl<S: Signaling + ?Sized + 'static> ActivationDriver<S> {
    pub fn new(signaling: Arc<S>) -> Arc<Self> {
        let mut rx = signaling.tunnel_hooks().subscribe_foreground();
        let initial_fg = *rx.borrow_and_update();
        let me = Arc::new(Self {
            signaling:      Arc::downgrade(&signaling),
            server_mode:    AtomicBool::new(false),
            foreground:     AtomicBool::new(initial_fg),
            active:         AtomicI64::new(0),
            last_push:      AtomicBool::new(false),
            reconcile_lock: std::sync::Mutex::new(()),
        });
        // Foreground subscriber.
        let weak = Arc::downgrade(&me);
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let fg = *rx.borrow_and_update();
                let Some(d) = weak.upgrade() else { return; };
                d.foreground.store(fg, Ordering::Release);
                d.reconcile();
            }
        });
        me.reconcile();
        me
    }

    /// Switch the driver's mode. Resets the live session
    /// counter so lingering watcher pushes from the previous
    /// mode don't affect the new mode's decision.
    pub fn set_mode(&self, mode: Mode) {
        self.server_mode.store(matches!(mode, Mode::Server), Ordering::Release);
        self.active.store(0, Ordering::Release);
        self.reconcile();
    }

    /// Bump the live session counter by `delta` (typically `+1`
    /// on Connected, `-1` on Disconnected). Clamps to `>= 0` so
    /// out-of-order Disconnected events from a previous mode
    /// can't drive the gate negative.
    pub fn bump_session(&self, delta: i64) {
        let _ = self.active.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |cur| Some((cur + delta).max(0)),
        );
        self.reconcile();
    }

    fn reconcile(&self) {
        // Serialise the want-compute + swap + push so concurrent
        // bumpers don't see a stale `last_push` after their swap.
        // Poisoned lock = panic in a previous reconcile; recover
        // the inner guard and continue — the gate's still useful.
        let _guard = self.reconcile_lock.lock()
            .unwrap_or_else(|e| e.into_inner());
        let want = if self.server_mode.load(Ordering::Acquire) {
            true
        } else {
            self.foreground.load(Ordering::Acquire)
                && self.active.load(Ordering::Acquire) <= 0
        };
        if self.last_push.swap(want, Ordering::AcqRel) == want { return; }
        if let Some(sig) = self.signaling.upgrade() {
            let h = sig.tunnel_hooks();
            if want { h.activate(); } else { h.deactivate(); }
        }
    }
}

// Manager → app `SessionEvent` notifications use the same lossless
// multi-consumer fan-out as signaling events: `lk_signaling::EventsSink`.

// ─── Client manager ────────────────────────────────────────────────────

/// Snapshot of the manager's single in-flight call. Owned by
/// [`ClientTunnelManager::current`]; replaced on each
/// `place_call`, taken on Drop or cleared by the per-call
/// watcher when `EngineEvent::Disconnected` arrives. The
/// `entered` flag is shared with the watcher so the manager's
/// CallEnded handler can distinguish pre-LK (treat WS CallEnded
/// as a Failed signal, tear the tunnel down) from post-LK (LK
/// is now authoritative, ignore WS CallEnded).
struct CurrentCall {
    peer:    PeerId,
    tunnel:  Arc<LkTunnel>,
    entered: Arc<AtomicBool>,
}

/// One outgoing call at a time. Re-calling `place_call` tears
/// down the previous session.
pub struct ClientTunnelManager<S: Signaling + ?Sized> {
    signaling: Arc<S>,
    sink:      EventsSink<SessionEvent>,
    /// Wrapped in `Arc` so the per-call watcher can clear its
    /// own entry on `Disconnected` (matches the server manager's
    /// `sessions` cleanup pattern). Apps no longer need an
    /// explicit `hang_up()` to drop the slot — tearing down the
    /// `LkTunnel` from the app side cascades to the watcher,
    /// which fires Disconnected, clears `current`, and drops
    /// the last tunnel reference.
    current:   Arc<Mutex<Option<CurrentCall>>>,
    driver:    Arc<ActivationDriver<S>>,
}

impl<S: Signaling + ?Sized> Drop for ClientTunnelManager<S> {
    fn drop(&mut self) {
        // Hang up any current session. The driver is untouched;
        // a subsequent manager will reconfigure it.
        if let Some(c) = self.current.lock().take() { c.tunnel.disconnect(); }
    }
}

impl<S: Signaling + ?Sized + 'static> ClientTunnelManager<S> {
    pub fn new(signaling: Arc<S>, driver: Arc<ActivationDriver<S>>) -> Arc<Self> {
        driver.set_mode(Mode::Client);
        let me = Arc::new(Self {
            signaling: signaling.clone(),
            sink:    EventsSink::new(),
            current: Arc::new(Mutex::new(None)),
            driver,
        });

        // Subscribe to Bale-side CallEnded once and apply the
        // "LK is the sole authority once the room is up" rule
        // centrally — apps never need to wire this themselves.
        // Pre-LK: tear the tunnel down so the watcher emits
        // `Failed`. Post-LK: log + ignore; a transient WS hiccup
        // must not drop a live session.
        // Consume the unified signaling events() stream (its own fan-out
        // subscriber) and act only on CallEnded — subscribed here at
        // construction, before any call is placed, so none can be missed.
        let mut events = signaling.events();
        let weak = Arc::downgrade(&me);
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                if let SignalingEvent::CallEnded { peer_id, reason } = ev {
                    let Some(me) = weak.upgrade() else { return; };
                    me.on_ws_call_ended(peer_id, reason);
                }
            }
        });

        me
    }

    pub fn events(&self) -> mpsc::UnboundedReceiver<SessionEvent> { self.sink.subscribe() }

    pub async fn place_call(&self, peer: PeerId) -> Result<(), PlaceCallError> {
        let transport = self.signaling.tunnel_hooks().place_call(peer.clone()).await?;
        let tunnel    = LkTunnel::connect(transport.url, transport.token);
        let entered   = Arc::new(AtomicBool::new(false));
        let new_call  = CurrentCall {
            peer:    peer.clone(),
            tunnel:  tunnel.clone(),
            entered: entered.clone(),
        };
        let old = self.current.lock().replace(new_call);
        if let Some(old) = old { old.tunnel.disconnect(); }
        spawn_client_watcher(
            self.sink.clone(), self.driver.clone(),
            self.current.clone(),
            peer, tunnel, entered,
        );
        Ok(())
    }

    pub fn signaling(&self) -> &Arc<S> { &self.signaling }

    /// CallEnded handler. Drops only the relevant subset of WS
    /// events — see `subscribe_call_ended` task above.
    fn on_ws_call_ended(&self, peer: PeerId, reason: EndReason) {
        let cur = self.current.lock();
        let Some(call) = cur.as_ref() else { return; };
        if call.peer != peer { return; }
        if call.entered.load(Ordering::Acquire) {
            log::info!("client: WS callEnded for {peer:?} (reason={reason:?}) — ignored (LK session live)");
            return;
        }
        log::info!("client: WS callEnded for {peer:?} (reason={reason:?}) — pre-LK, disconnecting tunnel");
        // tunnel.disconnect() fires `EngineEvent::Disconnected`,
        // the watcher sees `entered=false` and emits Failed.
        call.tunnel.disconnect();
    }
}

fn spawn_client_watcher<S>(
    sink:    EventsSink<SessionEvent>,
    driver:  Arc<ActivationDriver<S>>,
    current: Arc<Mutex<Option<CurrentCall>>>,
    peer:    PeerId,
    tunnel:  Arc<LkTunnel>,
    entered: Arc<AtomicBool>,
) where S: Signaling + ?Sized + 'static {
    let Some(mut events) = tunnel.events() else {
        log::warn!("spawn_client_watcher: events() already taken for {peer:?}");
        return;
    };
    let weak_t = Arc::downgrade(&tunnel);
    tokio::spawn(async move {
        // True iff `current` still points at our tunnel (we
        // haven't been superseded by a later `place_call`).
        // Critical to the emit-suppression below — same peer
        // id maps to two different consumer collectors across
        // a re-dial, and stale events from the old tunnel must
        // not wake the new collector.
        let still_current = || -> bool {
            let g = current.lock();
            g.as_ref()
                .and_then(|c| weak_t.upgrade().map(|t| Arc::ptr_eq(&c.tunnel, &t)))
                .unwrap_or(false)
        };
        // Atomically check + clear if still ours. Used in
        // Disconnected to evict our slot without racing a
        // successor's insert.
        let clear_if_current = || -> bool {
            let mut g = current.lock();
            let matches = g.as_ref()
                .and_then(|c| weak_t.upgrade().map(|t| Arc::ptr_eq(&c.tunnel, &t)))
                .unwrap_or(false);
            if matches { *g = None; }
            matches
        };
        while let Some(ev) = events.recv().await {
            match ev.kind {
                crate::EventKind::Connected if !entered.load(Ordering::Acquire) => {
                    entered.store(true, Ordering::Release);
                    if !still_current() {
                        // We were replaced by a later place_call
                        // while LK was bringing us to Connected.
                        // The new tunnel's watcher will be the
                        // authoritative source — disconnect
                        // ourselves so LK doesn't keep us up.
                        log::info!("client watcher: Connected on superseded tunnel {peer:?} — disconnecting, no emit");
                        if let Some(t) = weak_t.upgrade() { t.disconnect(); }
                        // Don't bump the session counter — the
                        // successor's Connected will bump for
                        // the *active* call. We're being
                        // discarded.
                        break;
                    }
                    driver.bump_session(1);
                    let Some(t) = weak_t.upgrade() else {
                        log::warn!("client watcher: weak upgrade failed at Connected for {peer:?}");
                        break;
                    };
                    log::info!("client watcher: emitting Connected for {peer:?}");
                    sink.emit(SessionEvent::Connected { peer_id: peer.clone(), tunnel: t });
                }
                crate::EventKind::Disconnected => {
                    let was_current = clear_if_current();
                    // Always rebalance the gate if we previously
                    // bumped up — this end-of-life cleanup is
                    // independent of whether the consumer cares
                    // about the event.
                    if entered.load(Ordering::Acquire) {
                        driver.bump_session(-1);
                    }
                    if !was_current {
                        // Superseded: a later `place_call`
                        // replaced our entry and disconnected
                        // us. The consumer is now awaiting the
                        // successor's events, not ours — emitting
                        // Failed/Disconnected here would wake
                        // them with a stale `peer_id` match and
                        // tear the new dial down.
                        log::info!("client watcher: superseded {peer:?} — suppressing emit");
                        break;
                    }
                    if entered.load(Ordering::Acquire) {
                        log::info!("client watcher: emitting Disconnected for {peer:?}");
                        sink.emit(SessionEvent::Disconnected { peer_id: peer.clone() });
                    } else {
                        // Never reached Connected — fail explicitly
                        // so the consumer can distinguish "failed
                        // to connect" from "was connected then
                        // dropped".
                        log::info!("client watcher: emitting Failed for {peer:?}");
                        sink.emit(SessionEvent::Failed { peer_id: peer.clone() });
                    }
                    break;
                }
                _ => {}
            }
        }
    });
}

// ─── Server manager ────────────────────────────────────────────────────

/// Map of peers with an in-flight `decide()` call. The sender's
/// `send(())` (or just `drop`) wakes the `select!` inside
/// `InternalHandler::decide` so the user's decider is abandoned
/// and we return `SilentlyIgnore`.
type PendingMap = Arc<Mutex<HashMap<PeerId, oneshot::Sender<()>>>>;

pub struct ServerTunnelManager<S: Signaling + ?Sized> {
    signaling: Arc<S>,
    sink:      EventsSink<SessionEvent>,
    sessions:  Arc<Mutex<HashMap<PeerId, Arc<LkTunnel>>>>,
    admission: Arc<Mutex<Option<Arc<dyn IncomingHandler>>>>,
    pending:   PendingMap,
    driver:    Arc<ActivationDriver<S>>,
}

impl<S: Signaling + ?Sized> Drop for ServerTunnelManager<S> {
    fn drop(&mut self) {
        // Disconnect every active session — peers see the LK
        // side drop. Driver untouched.
        let snapshot: Vec<Arc<LkTunnel>> = self.sessions.lock().values().cloned().collect();
        for t in snapshot { t.disconnect(); }
    }
}

impl<S: Signaling + ?Sized + 'static> ServerTunnelManager<S> {
    pub fn new(signaling: Arc<S>, driver: Arc<ActivationDriver<S>>) -> Arc<Self> {
        let admission = Arc::new(Mutex::new(None::<Arc<dyn IncomingHandler>>));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let me = Arc::new(Self {
            signaling: signaling.clone(),
            sink:      EventsSink::new(),
            sessions:  Arc::new(Mutex::new(HashMap::new())),
            admission: admission.clone(),
            pending:   pending.clone(),
            driver:    driver.clone(),
        });

        // Switch the driver to server mode — gate flips to
        // always-active and the session counter resets (any
        // client-mode bumps from before are no longer relevant).
        driver.set_mode(Mode::Server);

        signaling.tunnel_hooks().set_incoming_handler(Box::new(InternalHandler {
            admission: admission.clone(),
            pending:   pending.clone(),
        }));

        // Accepted-sessions drain — build a tunnel + registry
        // entry + watcher for each.
        let mut rx = signaling.tunnel_hooks().accepted_sessions();
        let sink     = me.sink.clone();
        let sessions = me.sessions.clone();
        tokio::spawn(async move {
            while let Some((peer, transport)) = rx.recv().await {
                let _ = &transport.peer_id;
                let tunnel = LkTunnel::connect_server(transport.url, transport.token);
                let old = sessions.lock().insert(peer.clone(), tunnel.clone());
                if let Some(old) = old { old.disconnect(); }
                spawn_server_watcher(
                    sink.clone(),
                    sessions.clone(),
                    peer,
                    tunnel,
                );
            }
        });

        // On user-initiated teardown (sign_out OR disconnect)
        // kill every active session so peers see clean LK drops
        // before the WS goes down.
        let mut td_rx = signaling.tunnel_hooks().subscribe_teardown();
        let sessions_for_td = me.sessions.clone();
        tokio::spawn(async move {
            while td_rx.changed().await.is_ok() {
                let snapshot: Vec<Arc<LkTunnel>> = sessions_for_td.lock().values().cloned().collect();
                for t in snapshot { t.disconnect(); }
            }
        });

        // Subscribe to Bale-side CallEnded and apply the
        // "LK is the sole authority" rule centrally:
        //   * peer with active LK session → ignored (LK is
        //     authoritative; the session's own watcher will
        //     surface Disconnected when LK actually drops).
        //   * peer with in-flight admission decision → cancel
        //     the decision (the user's notification clears) and
        //     surface Failed so the app drops its pending entry.
        // Consume the unified signaling events() stream (its own fan-out
        // subscriber) and act only on CallEnded — subscribed here at
        // construction, before any call is placed, so none can be missed.
        let mut events = signaling.events();
        let weak = Arc::downgrade(&me);
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                if let SignalingEvent::CallEnded { peer_id, reason } = ev {
                    let Some(me) = weak.upgrade() else { return; };
                    me.on_ws_call_ended(peer_id, reason);
                }
            }
        });

        me
    }

    /// CallEnded handler. See `subscribe_call_ended` task in
    /// `new` for the contract.
    fn on_ws_call_ended(&self, peer: PeerId, reason: EndReason) {
        if self.sessions.lock().contains_key(&peer) {
            log::info!("server: WS callEnded for {peer:?} (reason={reason:?}) — ignored (LK session live)");
            return;
        }
        // Not an active session — maybe a pending admission?
        // Removing from the map drops the sender; the `select!`
        // inside `InternalHandler::decide` sees its `oneshot::
        // Receiver` error out and returns `SilentlyIgnore` —
        // no explicit `send` needed.
        let was_pending = self.pending.lock().remove(&peer).is_some();
        if was_pending {
            log::info!("server: WS callEnded for {peer:?} (reason={reason:?}) — cancelling pending admission");
            self.sink.emit(SessionEvent::Failed { peer_id: peer });
        }
    }

    pub fn events(&self) -> mpsc::UnboundedReceiver<SessionEvent> { self.sink.subscribe() }

    pub fn set_admission(&self, handler: Arc<dyn IncomingHandler>) {
        *self.admission.lock() = Some(handler);
    }

    pub fn kick(&self, peer: &PeerId) {
        if let Some(t) = self.sessions.lock().get(peer).cloned() { t.disconnect(); }
    }

    pub fn disconnect_all(&self) {
        let snapshot: Vec<Arc<LkTunnel>> = self.sessions.lock().values().cloned().collect();
        for t in snapshot { t.disconnect(); }
    }

    pub fn signaling(&self) -> &Arc<S> { &self.signaling }
    pub fn driver(&self) -> &Arc<ActivationDriver<S>> { &self.driver }
}

fn spawn_server_watcher(
    sink:     EventsSink<SessionEvent>,
    registry: Arc<Mutex<HashMap<PeerId, Arc<LkTunnel>>>>,
    peer:     PeerId,
    tunnel:   Arc<LkTunnel>,
) {
    let Some(mut events) = tunnel.events() else {
        log::warn!("spawn_server_watcher: events() already taken for {peer:?}");
        return;
    };
    let weak_t = Arc::downgrade(&tunnel);
    tokio::spawn(async move {
        let mut entered = false;
        while let Some(ev) = events.recv().await {
            match ev.kind {
                crate::EventKind::Connected if !entered => {
                    entered = true;
                    let Some(t) = weak_t.upgrade() else {
                        log::warn!("server watcher: weak upgrade failed at Connected for {peer:?}");
                        break;
                    };
                    log::info!("server watcher: emitting Connected for {peer:?}");
                    sink.emit(SessionEvent::Connected { peer_id: peer.clone(), tunnel: t });
                }
                crate::EventKind::Disconnected => {
                    let mut map = registry.lock();
                    let matches = map.get(&peer)
                        .and_then(|cur| weak_t.upgrade().map(|t| Arc::ptr_eq(cur, &t)))
                        .unwrap_or(false);
                    if matches { map.remove(&peer); }
                    drop(map);
                    if entered {
                        log::info!("server watcher: emitting Disconnected for {peer:?}");
                        sink.emit(SessionEvent::Disconnected { peer_id: peer.clone() });
                    } else {
                        // Server-side: accepted but never reached
                        // Connected (handshake failed, peer never
                        // joined). Surface Failed so the consumer
                        // can clean up any pending UI for this peer.
                        log::info!("server watcher: emitting Failed for {peer:?}");
                        sink.emit(SessionEvent::Failed { peer_id: peer.clone() });
                    }
                    break;
                }
                _ => {}
            }
        }
    });
}

struct InternalHandler {
    admission: Arc<Mutex<Option<Arc<dyn IncomingHandler>>>>,
    pending:   PendingMap,
}

#[async_trait::async_trait]
impl IncomingHandler for InternalHandler {
    async fn decide(&self, peer: PeerId, display_name: Option<String>) -> CallDecision {
        let decider = self.admission.lock().clone();
        let Some(d) = decider else { return CallDecision::SilentlyIgnore; };

        // Register a cancel signal keyed by peer. A re-call
        // from the same peer replaces the previous entry — the
        // old sender drops, the old `decide`'s `select!` wakes
        // on RecvError and returns `SilentlyIgnore`. The
        // server's `on_ws_call_ended` also drops the sender on
        // Bale CallEnded for this peer.
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.pending.lock().insert(peer.clone(), cancel_tx);

        let result = tokio::select! {
            r = d.decide(peer.clone(), display_name) => r,
            // RecvError (sender dropped) or Ok(()) — either way
            // the manager wants this admission cancelled. The
            // user's decider future is dropped here; pure-Rust
            // deciders cancel cleanly, and the Kotlin JNI bridge
            // gets its parallel cleanup via the `SessionEvent::
            // Failed` the server's CallEnded handler emits.
            _ = cancel_rx => CallDecision::SilentlyIgnore,
        };

        // Clear our slot. If a re-call from the same peer raced
        // in and replaced our entry, this removes theirs too —
        // their select will then wake via RecvError and bail
        // SilentlyIgnore (acceptable; concurrent same-peer
        // decides should be near-impossible given bale-signaling's
        // own `in_flight_calls` dedup, and even if it happens the
        // user can just re-dial).
        self.pending.lock().remove(&peer);
        result
    }
}
