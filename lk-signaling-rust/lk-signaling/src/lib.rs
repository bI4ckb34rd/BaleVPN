//! lk-signaling — generic signaling abstraction.
//!
//! Defines the [`Signaling`] trait that any provider (Bale today,
//! others later) implements. Producing a [`TransportSession`] — a
//! LiveKit url + token + opaque peer id — is the entire job;
//! consumers feed the session into `lktunnel`'s `LkTunnel::connect`.
//!
//! Apps never see WS frames, RPC messages, or provider-specific
//! field names. Peer identity is opaque: a [`PeerId`] is an
//! `Arc<dyn PeerRef>` whose only inspectable surface is
//! [`PeerRef::id_str`], the stable string form used for config
//! round-trips via [`Signaling::resolve_peer`].

use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

// ─── Peer identity ──────────────────────────────────────────────────────

/// Opaque per-impl marker. Impls put whatever they need behind this
/// trait — for Bale, a numeric uid plus an accessHash; for another
/// provider, a UUID or public key. The trait surface gives apps no
/// way to read the contents, so peer identity is fully opaque to
/// consumers.
pub trait PeerRef: Send + Sync + 'static {
    /// Stable string form. Must round-trip through
    /// [`Signaling::resolve_peer`]. Apps use it to persist a peer
    /// in config files, CLI args, allow-lists, log lines.
    fn id_str(&self) -> &str;
}

/// Cheap-to-clone peer handle. Equality is by `Arc::ptr_eq` — the
/// impl must hand out the *same* `PeerId` for the same underlying
/// peer across its lifetime (i.e. cache by whatever internal id the
/// impl uses). If that invariant slips, per-peer app state (admission
/// counters, UI rows) silently splits across two handles for the
/// same peer.
#[derive(Clone)]
pub struct PeerId(Arc<dyn PeerRef>);

impl PeerId {
    pub fn new<R: PeerRef>(r: R) -> Self {
        Self(Arc::new(r))
    }

    /// Stable string form. Same as `Display`; provided as an
    /// inherent method for ergonomics in code that doesn't want to
    /// pull `fmt::Write` into scope.
    pub fn id_str(&self) -> &str {
        self.0.id_str()
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.id_str())
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerId({})", self.0.id_str())
    }
}

impl PartialEq for PeerId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for PeerId {}

impl Hash for PeerId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.0) as *const () as usize).hash(state)
    }
}

// ─── Sessions, decisions, events ────────────────────────────────────────

/// What apps feed into `LkTunnel::connect(url, token)`. The only
/// data that ever crosses the signaling/transport boundary into
/// apps.
#[derive(Clone)]
pub struct TransportSession {
    pub url:     String,
    pub token:   String,
    pub peer_id: PeerId,
}

/// What the app's [`IncomingHandler`] returns for each incoming
/// call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallDecision {
    /// Send the impl's accept message; the impl runs the wire-
    /// level accept and surfaces the resulting `(peer, transport)`
    /// pair on [`Signaling::accepted_sessions`].
    Accept,
    /// Send the impl's reject message; caller sees an explicit
    /// rejection.
    Reject,
    /// No response at all; caller times out. Used for blacklisted
    /// peers and for pending-call decision timeouts.
    SilentlyIgnore,
}

/// Why [`Signaling::place_call`] failed.
#[derive(Debug, Clone)]
pub enum PlaceCallError {
    /// Peer explicitly rejected the call.
    Rejected,
    /// Peer never joined the LK room within the impl's wait window.
    NoPeer,
    NotAuthenticated,
    Transport(String),
}

impl fmt::Display for PlaceCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected         => f.write_str("rejected by peer"),
            Self::NoPeer           => f.write_str("peer did not join"),
            Self::NotAuthenticated => f.write_str("not authenticated"),
            Self::Transport(s)     => write!(f, "transport: {s}"),
        }
    }
}
impl std::error::Error for PlaceCallError {}

/// Why a session ended. Reported on the [`Signaling::events`]
/// stream as part of [`SignalingEvent::CallEnded`] after the
/// peer hangs up, the network drops, or the call is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Rejected,
    CallerHangup,
    Timeout,
    NetworkDrop,
    Other(i32),
}

/// One page of contacts plus a continuation cursor.
#[derive(Default, Clone)]
pub struct ContactPage {
    pub peers:       Vec<PeerId>,
    /// `None` indicates no more pages. Opaque to the app — the impl
    /// can encode whatever it likes (server-side token, last-id,
    /// `(query_hash, offset)`, …).
    pub next_cursor: Option<String>,
}

// ─── Pluggable storage ──────────────────────────────────────────────────

/// Token persistence. Apps supply a platform-specific impl
/// (SharedPreferences / file with 0600 perms / OS keychain / …);
/// signaling reads on startup and writes on auth success or
/// refresh.
pub trait TokenStore: Send + Sync + 'static {
    fn load (&self) -> Option<Vec<u8>>;
    fn save (&self, bytes: &[u8]);
    fn clear(&self);
}

// ─── Handlers + the trait ───────────────────────────────────────────────

/// Decide what to do with each incoming call. Async — the app can
/// race against its own UI / admission policy / pending-call timer.
/// Returning [`CallDecision::SilentlyIgnore`] is how blacklists and
/// pending-call timeouts are expressed.
///
/// `display_name` is the impl's best-effort resolution at call
/// time. If the impl hasn't seen this peer yet, it's `None` —
/// the handler can call [`Signaling::fetch_display_name`] for
/// a guaranteed lookup.
#[async_trait::async_trait]
pub trait IncomingHandler: Send + Sync + 'static {
    async fn decide(&self, peer: PeerId, display_name: Option<String>) -> CallDecision;
}

/// Generic signaling errors. Most errors that matter to apps get
/// their own typed return ([`PlaceCallError`], [`EndReason`]); this
/// is the catch-all for the rest.
#[derive(Debug, Clone)]
pub enum SignalingError {
    NotAuthenticated,
    NotConnected,
    NotSupported,
    Transport(String),
}

impl fmt::Display for SignalingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAuthenticated => f.write_str("not authenticated"),
            Self::NotConnected     => f.write_str("not connected"),
            Self::NotSupported     => f.write_str("not supported by this signaling impl"),
            Self::Transport(s)     => write!(f, "transport: {s}"),
        }
    }
}
impl std::error::Error for SignalingError {}

/// Async events the signaling impl reports to its consumer.
/// Surfaced on a single stream via [`Signaling::events`] so
/// consumers subscribe once and pattern-match.
#[derive(Debug, Clone)]
pub enum SignalingEvent {
    /// Token was rejected by the server (or never valid). App
    /// should clear its [`TokenStore`] and prompt re-auth.
    AuthExpired,
    /// Server protocol/version is incompatible with this build.
    /// Retry won't help — the app needs to be updated.
    ProtocolObsolete,
    /// Any call this signaling instance was tracking ended —
    /// regardless of direction. The app discriminates
    /// outgoing-vs-incoming from its own per-peer state.
    CallEnded { peer_id: PeerId, reason: EndReason },
    /// The impl's backing contact list changed (server push,
    /// blocked-user removal, etc.). UI should re-fetch.
    ContactsChanged,
}

#[async_trait::async_trait]
pub trait Signaling: Send + Sync {
    // ── Connection lifecycle ───────────────────────────────────────

    /// Whether the impl's control channel is currently up.
    fn is_connected(&self) -> bool;

    /// Bring the control channel up. Idempotent.
    async fn connect(&self) -> Result<(), SignalingError>;

    /// Tear the control channel down. Auth state is preserved; a
    /// subsequent [`Self::connect`] reuses the stored token.
    async fn disconnect(&self);

    /// Clear stored credentials and tear down. Wakes
    /// [`TunnelHooks::subscribe_sign_out`] subscribers before
    /// proceeding so managers can disconnect their sessions
    /// cleanly first. The WS / control channel comes down as a
    /// consequence of the auth state going away.
    async fn sign_out(&self);

    /// Stream of async events — auth expired, protocol obsolete,
    /// call ended, contacts changed. Single-consumer: calling
    /// again replaces the receiver, no backlog kept for an
    /// absent consumer.
    fn events(&self) -> tokio::sync::mpsc::UnboundedReceiver<SignalingEvent>;

    // ── Auth state ─────────────────────────────────────────────────

    fn is_authenticated(&self) -> bool;

    /// Who we're signed in as. `None` if not authenticated.
    async fn whoami(&self) -> Option<PeerId>;

    // ── App lifecycle hint ─────────────────────────────────────────

    /// App lifecycle (foreground vs background). Apps push this
    /// from `ProcessLifecycleOwner` (or equivalent). The trait
    /// doesn't gate the control channel on this directly — the
    /// lktunnel managers subscribe via
    /// [`TunnelHooks::subscribe_foreground`] and combine with
    /// their own state to drive `activate` / `deactivate`.
    fn set_foreground(&self, _foreground: bool) {}

    // ── Tunnel-manager hooks accessor ──────────────────────────────

    /// Borrow the [`TunnelHooks`] surface for this impl. Apps
    /// don't normally touch this — it's the internal contract
    /// the lktunnel managers use to drive the control channel
    /// lifecycle (`activate`/`deactivate`), drain accepted
    /// incoming sessions, and react to sign-out + foreground
    /// changes.
    fn tunnel_hooks(&self) -> &dyn TunnelHooks;

    // ── Contacts ───────────────────────────────────────────────────
    //
    // Impls without a contact concept return empty pages and a
    // `NotSupported` error from the mutators.

    /// Fetch one page of contacts. `query` filters server-side if
    /// the impl supports it; `cursor` continues from a previous
    /// page (pass `None` to start). `limit` is a hint, not a
    /// guarantee — the impl may return fewer.
    async fn list_contacts(
        &self,
        query:  Option<&str>,
        cursor: Option<&str>,
        limit:  usize,
    ) -> Result<ContactPage, SignalingError>;

    /// Look up peer(s) globally by phone number. Returns 0 or 1
    /// matches (occasionally more for number-collisions across
    /// providers). The split into a separate
    /// [`Self::add_to_contacts`] step lets apps preview matches
    /// before committing — but note that some impls (Bale's
    /// `ImportContacts` is one) **add as a side effect of search**
    /// because the underlying RPC conflates the two. Consumers
    /// targeting any-impl should treat search as potentially
    /// mutating and explicitly call `add_to_contacts` to make
    /// intent clear at the call site.
    ///
    /// Impls without global phone lookup return
    /// [`SignalingError::NotSupported`].
    async fn search_contact_by_phone(&self, phone: &str)
        -> Result<Vec<PeerId>, SignalingError>;

    /// Commit a peer found via [`Self::search_contact_by_phone`]
    /// to the local contact list. On Bale this is a no-op
    /// because the search RPC already adds; on impls where add
    /// is a distinct step, this is where it happens.
    async fn add_to_contacts(&self, peer: &PeerId)
        -> Result<(), SignalingError>;

    /// Remove a peer from the contact list. Impls without
    /// contacts return [`SignalingError::NotSupported`].
    async fn remove_contact(&self, p: &PeerId) -> Result<(), SignalingError>;

    // ── Lookup / diagnostics ───────────────────────────────────────

    /// Cached, synchronous display-name lookup. Returns `None` if
    /// the impl hasn't seen this peer yet — call
    /// [`Self::fetch_display_name`] to populate the cache.
    fn peer_display_name(&self, p: &PeerId) -> Option<String>;

    /// First-time fetch over RPC. Populates the cache that
    /// [`Self::peer_display_name`] reads from.
    async fn fetch_display_name(&self, p: &PeerId) -> Option<String>;

    /// Reverse of [`PeerId::id_str`] → handle. Used to hydrate a
    /// `PeerId` from a stored string (config file, CLI arg,
    /// persisted allow-list). Returns `None` if the impl can't
    /// resolve the string to a known peer.
    async fn resolve_peer(&self, s: &str) -> Option<PeerId>;
}

// ─── Tunnel-manager hooks ──────────────────────────────────────────────

/// Internal contract between a [`Signaling`] impl and the
/// lktunnel managers (`ClientTunnelManager` / `ServerTunnelManager`).
/// Apps don't normally see this — they hand the manager an
/// `Arc<dyn Signaling>` and the manager grabs the hooks via
/// [`Signaling::tunnel_hooks`].
///
/// Splitting it out of [`Signaling`] keeps the app-facing
/// surface (auth, contacts, place_call, events) separate from
/// the manager-only plumbing (activate/deactivate, accepted
/// session drain, foreground / sign-out subscriptions).
#[async_trait::async_trait]
pub trait TunnelHooks: Send + Sync {
    /// Place a call to `peer`. Returns once the wire-level
    /// place-call completes (LK creds in hand); the peer's
    /// accept/reject decision may still race and arrive after
    /// this returns — that case surfaces on
    /// [`Signaling::events`] as [`SignalingEvent::CallEnded`]
    /// with [`EndReason::Rejected`]. The
    /// [`crate::ClientTunnelManager`](../../lktunnel/manager/struct.ClientTunnelManager.html)
    /// wraps this; apps don't normally call it directly.
    async fn place_call(&self, peer: PeerId) -> Result<TransportSession, PlaceCallError>;

    /// Install the handler that decides what to do with each
    /// incoming call. Replaces any previous handler. After the
    /// handler returns [`CallDecision::Accept`], the impl runs
    /// the wire-level accept and surfaces the resulting
    /// `(peer, transport)` pair on [`Self::accepted_sessions`].
    /// `ServerTunnelManager` installs its own wrapper here;
    /// apps install policy via the manager's `set_admission`.
    fn set_incoming_handler(&self, h: Box<dyn IncomingHandler>);

    /// Stream of accepted incoming sessions. Each fires after
    /// the impl's [`IncomingHandler`] returned `Accept` and the
    /// wire-level accept succeeded — `ServerTunnelManager`
    /// builds the actual transport from the
    /// [`TransportSession`]. Single-consumer.
    fn accepted_sessions(&self) -> tokio::sync::mpsc::UnboundedReceiver<(PeerId, TransportSession)>;

    /// "I want the control channel up right now." Paired calls
    /// from the manager — `activate` when no session is active
    /// (or, in server mode, always), `deactivate` during a live
    /// tunnel (client) / on manager drop. Defaults to active
    /// when no manager has ever pushed.
    fn activate(&self);
    fn deactivate(&self);

    /// Subscribe to foreground-state changes. Fires every time
    /// the app pushes [`Signaling::set_foreground`]; impls
    /// without app lifecycle hand back a receiver pinned at
    /// `true`.
    fn subscribe_foreground(&self) -> tokio::sync::watch::Receiver<bool> {
        static DEFAULT_FG: once_cell::sync::OnceCell<tokio::sync::watch::Sender<bool>>
            = once_cell::sync::OnceCell::new();
        DEFAULT_FG.get_or_init(|| tokio::sync::watch::channel(true).0).subscribe()
    }

    /// Subscribe to user-initiated teardown events. The value
    /// is a monotonic counter — both [`Signaling::sign_out`] and
    /// [`Signaling::disconnect`] increment it. Subscribers wait
    /// on `changed()` and react: `ServerTunnelManager`
    /// `disconnect_all()`s its sessions so peers see clean LK
    /// drops; `ClientTunnelManager` could optionally `hang_up`
    /// any in-flight call.
    ///
    /// Distinct from foreground/background — this fires only on
    /// explicit user intent (the UI's "Disconnect" / "Sign Out"
    /// buttons), not on lifecycle transitions.
    fn subscribe_teardown(&self) -> tokio::sync::watch::Receiver<u64> {
        static DEFAULT_TD: once_cell::sync::OnceCell<tokio::sync::watch::Sender<u64>>
            = once_cell::sync::OnceCell::new();
        DEFAULT_TD.get_or_init(|| tokio::sync::watch::channel(0u64).0).subscribe()
    }

    /// Manager-internal CallEnded fan-out. Returns a fresh
    /// multi-subscriber receiver — every call gets its own
    /// independent stream (no replace-on-subscribe). The
    /// `ClientTunnelManager` / `ServerTunnelManager` each
    /// subscribe at construction so they can enforce the
    /// "LK-is-the-sole-authority once both parties joined the
    /// room" rule centrally:
    ///   * client manager: pre-LK CallEnded for the dialed
    ///     peer → tear the tunnel down (watcher emits `Failed`);
    ///     post-LK → ignored (a transient WS hiccup must not
    ///     drop a live session).
    ///   * server manager: CallEnded for a peer with an active
    ///     LK session → ignored; for a peer with a pending
    ///     admission decision → cancel the decision so the
    ///     app's notification clears.
    ///
    /// Distinct from the unified [`Signaling::events`] stream
    /// (which is single-consumer for app use): subscribers here
    /// don't interfere with apps that have their own
    /// `events()` consumer. Impls without call lifecycle return
    /// the default — an immediately-closed stream that yields
    /// `None` on first `recv`.
    fn subscribe_call_ended(&self) -> tokio::sync::mpsc::UnboundedReceiver<(PeerId, EndReason)> {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // `_tx` drops immediately → `rx.recv()` returns `None`
        // straight away. Impls without call lifecycle (test
        // doubles, future protocol bridges that don't model
        // calls) get this freebie.
        rx
    }
}

#[cfg(test)]
mod tests;
