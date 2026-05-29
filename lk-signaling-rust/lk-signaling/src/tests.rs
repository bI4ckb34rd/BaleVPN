//! Sanity tests for the trait crate. The bulk of the testing for
//! any given impl lives next to that impl; here we only verify the
//! contract types behave the way the trait documents.

use super::*;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::mpsc;

// ─── Test impl ──────────────────────────────────────────────────────────

/// Minimal in-memory peer ref. Identity is just a numeric id.
struct FakePeer { id: String }
impl PeerRef for FakePeer {
    fn id_str(&self) -> &str { &self.id }
}

/// Minimal Signaling impl. Real impls cache peer handles to honour
/// the ptr-eq identity invariant; this one does the same with a
/// `HashMap<String, PeerId>`.
struct FakeSignaling {
    peers:     Mutex<HashMap<String, PeerId>>,
    events_tx: Mutex<Option<mpsc::UnboundedSender<SignalingEvent>>>,
}

impl FakeSignaling {
    fn new() -> Self {
        Self {
            peers:     Mutex::new(HashMap::new()),
            events_tx: Mutex::new(None),
        }
    }

    /// Look up an existing handle or mint a new one. This is the
    /// peer cache that enforces ptr-eq identity.
    fn intern(&self, id: &str) -> PeerId {
        let mut map = self.peers.lock().unwrap();
        if let Some(existing) = map.get(id) {
            return existing.clone();
        }
        let p = PeerId::new(FakePeer { id: id.to_string() });
        map.insert(id.to_string(), p.clone());
        p
    }

    fn emit(&self, ev: SignalingEvent) {
        if let Some(tx) = self.events_tx.lock().unwrap().as_ref() {
            let _ = tx.send(ev);
        }
    }
}

#[async_trait::async_trait]
impl Signaling for FakeSignaling {
    fn is_connected(&self) -> bool { true }
    async fn connect(&self) -> Result<(), SignalingError> { Ok(()) }
    async fn disconnect(&self) {}
    async fn sign_out(&self) {}

    fn events(&self) -> mpsc::UnboundedReceiver<SignalingEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.events_tx.lock().unwrap() = Some(tx);
        rx
    }

    fn is_authenticated(&self) -> bool { true }
    async fn whoami(&self) -> Option<PeerId> { Some(self.intern("self")) }

    fn tunnel_hooks(&self) -> &dyn TunnelHooks { self }

    async fn list_contacts(
        &self,
        _query:  Option<&str>,
        _cursor: Option<&str>,
        _limit:  usize,
    ) -> Result<ContactPage, SignalingError> {
        Ok(ContactPage::default())
    }
    async fn search_contact_by_phone(&self, _: &str) -> Result<Vec<PeerId>, SignalingError> {
        Err(SignalingError::NotSupported)
    }
    async fn add_to_contacts(&self, _: &PeerId) -> Result<(), SignalingError> {
        Err(SignalingError::NotSupported)
    }
    async fn remove_contact(&self, _: &PeerId) -> Result<(), SignalingError> {
        Err(SignalingError::NotSupported)
    }

    fn peer_display_name(&self, _: &PeerId) -> Option<String> { None }
    async fn fetch_display_name(&self, _: &PeerId) -> Option<String> { None }
    async fn resolve_peer(&self, s: &str) -> Option<PeerId> { Some(self.intern(s)) }
}

#[async_trait::async_trait]
impl TunnelHooks for FakeSignaling {
    async fn place_call(&self, peer: PeerId) -> Result<TransportSession, PlaceCallError> {
        Ok(TransportSession {
            url:     "wss://fake".into(),
            token:   "fake-token".into(),
            peer_id: peer,
        })
    }
    fn set_incoming_handler(&self, _: Box<dyn IncomingHandler>) {}
    fn accepted_sessions(&self) -> mpsc::UnboundedReceiver<(PeerId, TransportSession)> {
        let (_tx, rx) = mpsc::unbounded_channel();
        rx
    }
    fn activate  (&self) {}
    fn deactivate(&self) {}
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[test]
fn peer_id_display_round_trips_through_id_str() {
    let p = PeerId::new(FakePeer { id: "1234567890".into() });
    assert_eq!(p.id_str(), "1234567890");
    assert_eq!(format!("{p}"), "1234567890");
    assert_eq!(format!("{p:?}"), "PeerId(1234567890)");
}

#[test]
fn peer_id_equality_is_ptr_eq() {
    let a = PeerId::new(FakePeer { id: "x".into() });
    let b = PeerId::new(FakePeer { id: "x".into() });   // same id, different Arc
    assert_ne!(a, b);
    assert_eq!(a, a.clone());
}

#[tokio::test]
async fn resolve_peer_returns_same_handle_for_same_id() {
    let s = FakeSignaling::new();
    let a = s.resolve_peer("42").await.unwrap();
    let b = s.resolve_peer("42").await.unwrap();
    assert_eq!(a, b, "impl must hand out the same PeerId for the same underlying peer");
}

#[tokio::test]
async fn resolve_peer_distinct_ids_distinct_handles() {
    let s = FakeSignaling::new();
    let a = s.resolve_peer("1").await.unwrap();
    let b = s.resolve_peer("2").await.unwrap();
    assert_ne!(a, b);
}

#[tokio::test]
async fn place_call_returns_transport_session_with_same_peer_handle() {
    let s = FakeSignaling::new();
    let p = s.resolve_peer("99").await.unwrap();
    let sess = s.place_call(p.clone()).await.unwrap();
    assert_eq!(sess.peer_id, p);
    assert_eq!(sess.url,   "wss://fake");
    assert_eq!(sess.token, "fake-token");
}

#[tokio::test]
async fn incoming_handler_decide_basic() {
    struct AlwaysAccept;
    #[async_trait::async_trait]
    impl IncomingHandler for AlwaysAccept {
        async fn decide(&self, _peer: PeerId, _display_name: Option<String>) -> CallDecision {
            CallDecision::Accept
        }
    }
    let h: Box<dyn IncomingHandler> = Box::new(AlwaysAccept);
    let s = FakeSignaling::new();
    let p = s.resolve_peer("7").await.unwrap();
    let decision = h.decide(p, None).await;
    assert_eq!(decision, CallDecision::Accept);
}

#[tokio::test]
async fn events_stream_carries_emitted_events() {
    let s = FakeSignaling::new();
    let mut rx = s.events();
    s.emit(SignalingEvent::AuthExpired);
    let ev = rx.recv().await.expect("events stream should emit");
    assert!(matches!(ev, SignalingEvent::AuthExpired));
}

#[tokio::test]
async fn not_supported_propagates() {
    let s = FakeSignaling::new();
    let p = s.resolve_peer("x").await.unwrap();
    assert!(matches!(s.remove_contact(&p).await,                Err(SignalingError::NotSupported)));
    assert!(matches!(s.search_contact_by_phone("+1").await,     Err(SignalingError::NotSupported)));
    assert!(matches!(s.add_to_contacts(&p).await,               Err(SignalingError::NotSupported)));
}

#[test]
fn place_call_error_displays() {
    assert_eq!(format!("{}", PlaceCallError::Rejected),         "rejected by peer");
    assert_eq!(format!("{}", PlaceCallError::NoPeer),           "peer did not join");
    assert_eq!(format!("{}", PlaceCallError::NotAuthenticated), "not authenticated");
    assert_eq!(format!("{}", PlaceCallError::Transport("x".into())), "transport: x");
}

// Compile-time check: PeerId clones are cheap (one Arc clone =
// one atomic inc), and the trait can be put behind `dyn`. If
// either of these regresses, this stops compiling.
#[allow(dead_code)]
fn _object_safety_and_clone() {
    fn assert_dyn(_: &dyn Signaling) {}
    fn assert_clone<T: Clone>() {}
    assert_clone::<PeerId>();
    let _s: Option<Box<dyn Signaling>> = None;
    let _: Option<&dyn Signaling> = None.as_ref().map(|s: &Box<dyn Signaling>| s.as_ref());
    assert_dyn as fn(&dyn Signaling);
}
