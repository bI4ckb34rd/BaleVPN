//! Signaling-side JNI surface (formerly `bale-signaling-android`).
//!
//! Symbols:
//!   * `Java_ai_bale_proxy_bale_BaleAuthNative_*`     — static auth flow
//!   * `Java_ai_bale_proxy_bale_BaleSignalingNative_*` — handle-based
//!
//! Bridges the async Rust API to sync Java entry points (the
//! Kotlin side wraps each native call in a Kotlin coroutine if
//! needed) and marshals `TokenStore` calls back to Java via a
//! small Kotlin-defined interface.
//!
//! The Java handle is a `jlong` pointing into the [`REG`] handle
//! registry of `Arc<BaleSignaling>`s. Use [`with_signaling`] to
//! borrow it.
//!
//! `JNI_OnLoad` lives in the crate root (`lib.rs`); this module
//! exposes [`init_class_refs`] which the unified loader calls to
//! cache module-local class GlobalRefs.

use std::sync::Arc;

use bale_signaling::auth::{AuthOutcome, BaleAuth};
use bale_signaling::BaleSignaling;
use jni::objects::{GlobalRef, JClass, JMethodID, JObject, JString, JValue};
use jni::signature::{Primitive, ReturnType};
use jni::sys::{jboolean, jint, jlong, jobject, jstring, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;
use lk_signaling::{
    CallDecision, EndReason, IncomingHandler, PeerId,
    Signaling, SignalingEvent, TokenStore,
};

// Both `runtime()` and `vm()` live in [`jni_shared`] (process-wide
// singletons used by the whole `.so`). Re-export shortens call sites.
use jni_shared::{runtime, vm};

/// Cached `NativeContactPage` class.
static NATIVE_CONTACT_PAGE_CLS: once_cell::sync::OnceCell<jni::objects::GlobalRef>
    = once_cell::sync::OnceCell::new();

/// Called by the unified `JNI_OnLoad` to cache class GlobalRefs
/// that this module needs. Must run from the `System.loadLibrary`
/// thread (where the app classloader is current).
pub(crate) fn init_class_refs(mut env: JNIEnv<'_>) {
    for (cell, name) in [
        (&NATIVE_CONTACT_PAGE_CLS,      "ai/bale/proxy/bale/NativeContactPage"),
    ] {
        match env.find_class(name) {
            Ok(cls) => match env.new_global_ref(cls) {
                Ok(g)  => { let _ = cell.set(g); }
                Err(e) => log::warn!("JNI_OnLoad: GlobalRef for {name}: {e}"),
            },
            Err(e) => log::warn!("JNI_OnLoad: find_class {name}: {e}"),
        }
    }
}

// ─── Handle registry ─────────────────────────────────────────────────────
//
// Replaces the raw `Box<Arc<BaleSignaling>>` pointer pattern.
// Each JNI call looks up the handle in a `HashMap<u64,
// Arc<BaleSignaling>>` under a brief Mutex, clones the Arc,
// runs the closure with the cloned Arc as owner, then drops.
// `nativeDestroy` removes the entry from the map — any
// in-flight calls keep their own clone until they return, so
// concurrent JNI calls are safe even during destroy. This
// means the Kotlin side doesn't need to @Synchronize each
// native call against destroy.

/// Process-wide handle registry — `jlong` Kotlin handles map to
/// `Arc<BaleSignaling>`. Shares the [`jni_shared::HandleRegistry`]
/// pattern with the lktunnel shim; see that crate's docs for the
/// per-call Arc-clone + atomic remove_and_take semantics.
pub(crate) static REG: jni_shared::RegistryHandle<BaleSignaling> =
    jni_shared::once_cell::sync::Lazy::new(jni_shared::HandleRegistry::new);

fn box_signaling(s: Arc<BaleSignaling>) -> jlong {
    REG.insert(s) as jlong
}

fn with_signaling<F, R>(handle: jlong, f: F) -> Option<R>
where F: FnOnce(&Arc<BaleSignaling>) -> R
{
    let arc = REG.lookup(handle as u64)?;
    Some(f(&arc))
}

unsafe fn drop_signaling(handle: jlong) {
    REG.remove(handle as u64);
}

// String marshaling helpers live in jni_shared so the lktunnel
// shim can use them too. Re-export so call sites stay short.
use jni_shared::{jstr, jstring_new};

// ─── TokenStore bridge ──────────────────────────────────────────────────

/// `bale-signaling` calls `TokenStore::load / save / clear`; the
/// Kotlin side provides an implementation (e.g.
/// SharedPreferences-backed) and we marshal each call back to
/// Java via the held global ref.
///
/// Holds a `GlobalRef` so the Java object survives the JNI call
/// boundary; thread-attach happens lazily inside each method.
struct JavaTokenStore {
    store: jni::objects::GlobalRef,
}

impl JavaTokenStore {
    fn new(env: &mut JNIEnv, store: JObject) -> Result<Self, jni::errors::Error> {
        Ok(Self { store: env.new_global_ref(store)? })
    }

    /// Attach the current thread to the JVM, run `body` with an
    /// env handle. Returns `None` if attach fails (e.g. the JVM
    /// is shutting down).
    fn with_env<F, R>(&self, f: F) -> Option<R>
    where F: FnOnce(&mut JNIEnv, &JObject) -> Option<R>
    {
        let mut env = vm().attach_current_thread_permanently().ok()?;
        f(&mut env, self.store.as_obj())
    }
}

impl TokenStore for JavaTokenStore {
    fn load(&self) -> Option<Vec<u8>> {
        // with_env already unwraps the inner Option; chaining
        // another .flatten() would double-wrap.
        self.with_env(|env, store| {
            let result = env.call_method(store, "load", "()[B", &[]).ok()?;
            let array = result.l().ok()?;
            if array.is_null() { return None; }
            env.convert_byte_array(jni::objects::JByteArray::from(array)).ok()
        })
    }

    fn save(&self, bytes: &[u8]) {
        let _ = self.with_env(|env, store| {
            let byte_array = env.byte_array_from_slice(bytes).ok()?;
            let _ = env.call_method(store, "save", "([B)V",
                &[(&JObject::from(byte_array)).into()]);
            Some(())
        });
    }

    fn clear(&self) {
        let _ = self.with_env(|env, store| {
            let _ = env.call_method(store, "clear", "()V", &[]);
            Some(())
        });
    }
}

// ─── SignalingEvent listener bridge ──────────────────────────────────────

/// Bridges the unified [`SignalingEvent`] stream into the
/// Kotlin `SignalingEventListener` interface:
///
/// ```kotlin
/// interface SignalingEventListener {
///     fun onAuthExpired()
///     fun onProtocolObsolete()
///     fun onCallEnded    (peerId: String, endReasonCode: Int)
///     fun onContactsChanged()
/// }
/// ```
///
/// Method IDs are cached at attach so each fire is one JNI call.
struct JavaSignalingObserver {
    obs:                  GlobalRef,
    on_auth_expired:      JMethodID,
    on_protocol_obsolete: JMethodID,
    on_call_ended:        JMethodID,
    on_contacts_changed:  JMethodID,
}

impl JavaSignalingObserver {
    fn new(env: &mut JNIEnv, observer: JObject) -> Result<Self, jni::errors::Error> {
        let cls = env.get_object_class(&observer)?;
        let on_auth_expired      = env.get_method_id(&cls, "onAuthExpired",     "()V")?;
        let on_protocol_obsolete = env.get_method_id(&cls, "onProtocolObsolete","()V")?;
        let on_call_ended        = env.get_method_id(&cls, "onCallEnded",
            "(Ljava/lang/String;I)V")?;
        let on_contacts_changed  = env.get_method_id(&cls, "onContactsChanged", "()V")?;
        Ok(Self {
            obs: env.new_global_ref(observer)?,
            on_auth_expired, on_protocol_obsolete,
            on_call_ended, on_contacts_changed,
        })
    }

    fn fire_void(&self, mid: JMethodID) {
        let Ok(mut env) = vm().attach_current_thread_permanently() else { return; };
        unsafe {
            let _ = env.call_method_unchecked(
                self.obs.as_obj(), mid, ReturnType::Primitive(Primitive::Void), &[],
            );
        }
    }

    fn fire_call_ended(&self, peer_id: &str, end_code: i32) {
        let Ok(mut env) = vm().attach_current_thread_permanently() else { return; };
        let Ok(pid) = env.new_string(peer_id) else { return; };
        let jvals = [JValue::Object(&pid).as_jni(), JValue::Int(end_code).as_jni()];
        unsafe {
            let _ = env.call_method_unchecked(
                self.obs.as_obj(), self.on_call_ended,
                ReturnType::Primitive(Primitive::Void), &jvals,
            );
        }
    }

    fn dispatch(&self, ev: SignalingEvent) {
        match ev {
            SignalingEvent::AuthExpired      => self.fire_void(self.on_auth_expired),
            SignalingEvent::ProtocolObsolete => self.fire_void(self.on_protocol_obsolete),
            SignalingEvent::ContactsChanged  => self.fire_void(self.on_contacts_changed),
            SignalingEvent::CallEnded { peer_id, reason } => {
                self.fire_call_ended(peer_id.id_str(), end_reason_code(&reason));
            }
        }
    }
}

/// `EndReason` → integer code shared with Kotlin. Keep in sync
/// with the matching `EndReason` enum on the Kotlin side. Using
/// numbers because tagged-union marshalling across JNI is more
/// work than the readability buys.
fn end_reason_code(r: &EndReason) -> i32 {
    match r {
        EndReason::Rejected       => 0,
        EndReason::CallerHangup   => 1,
        EndReason::Timeout        => 2,
        EndReason::NetworkDrop    => 3,
        EndReason::Other(code)    => 100 + *code,    // disjoint range
    }
}

// ─── IncomingHandler bridge ─────────────────────────────────────────────

/// Java `JavaIncomingHandler.decide(peerId, displayName?) -> int`
/// where the int is `0 = Accept`, `1 = Reject`, `2 = SilentlyIgnore`.
/// Anything else maps to `SilentlyIgnore` (safer default — caller
/// times out instead of being explicitly rejected).
struct JavaIncomingHandlerBridge {
    handler: GlobalRef,
    decide:  JMethodID,
}

impl JavaIncomingHandlerBridge {
    fn new(env: &mut JNIEnv, handler: JObject) -> Result<Self, jni::errors::Error> {
        let cls    = env.get_object_class(&handler)?;
        let decide = env.get_method_id(&cls, "decide",
            "(Ljava/lang/String;Ljava/lang/String;)I")?;
        Ok(Self { handler: env.new_global_ref(handler)?, decide })
    }
}

#[async_trait::async_trait]
impl IncomingHandler for JavaIncomingHandlerBridge {
    async fn decide(&self, peer: PeerId, display_name: Option<String>) -> CallDecision {
        // The Java decide() is sync; run it on the blocking
        // pool so we don't pin a tokio worker for the duration
        // of the user's UI prompt.
        let handler = self.handler.clone();
        let decide  = self.decide;
        let peer_id = peer.id_str().to_string();
        let name    = display_name;

        let code = tokio::task::spawn_blocking(move || -> Option<i32> {
            let mut env = vm().attach_current_thread_permanently().ok()?;
            let pid     = env.new_string(&peer_id).ok()?;
            let display = match name {
                Some(s) => Some(env.new_string(&s).ok()?),
                None    => None,
            };
            let null_obj = JObject::null();
            let display_ref = display.as_ref().map(AsRef::as_ref).unwrap_or(&null_obj);
            let jvals = [JValue::Object(&pid).as_jni(), JValue::Object(display_ref).as_jni()];
            let result = unsafe {
                env.call_method_unchecked(
                    handler.as_obj(), decide,
                    ReturnType::Primitive(Primitive::Int), &jvals,
                )
            }.ok()?;
            result.i().ok()
        }).await.ok().flatten();

        match code {
            Some(0) => CallDecision::Accept,
            Some(1) => CallDecision::Reject,
            _       => CallDecision::SilentlyIgnore,
        }
    }
}

// ─── BaleSignaling lifecycle ────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeCreate<'l>(
    mut env:     JNIEnv<'l>,
    _cls:        JClass<'l>,
    token_store: JObject<'l>,
    observer:    JObject<'l>,
) -> jlong {
    let store = match JavaTokenStore::new(&mut env, token_store) {
        Ok(s)  => Arc::new(s) as Arc<dyn TokenStore>,
        Err(e) => { log::warn!("nativeCreate: token store ref failed: {e}"); return 0; }
    };
    let obs = match JavaSignalingObserver::new(&mut env, observer) {
        Ok(o)  => Arc::new(o),
        Err(e) => { log::warn!("nativeCreate: observer ref failed: {e}"); return 0; }
    };
    let sig = BaleSignaling::new(store);

    // Subscribe to the unified events stream once at construction
    // and forward each one to the Kotlin observer interface.
    let mut events = sig.events();
    let o = obs.clone();
    jni_shared::runtime().spawn(async move {
        while let Some(ev) = events.recv().await { o.dispatch(ev); }
    });

    box_signaling(sig)
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeDestroy<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) {
    // SAFETY: Kotlin's per-instance monitor ensures no other call
    // is in flight against this handle. See `with_signaling`.
    unsafe { drop_signaling(handle); }
}

// `nativeSubscribeSessions` removed — accepted incoming sessions
// flow through `lktunnel::manager::ServerTunnelManager` now, whose
// JNI lives in `manager.rs` (`nativeServerSubscribe`).

/// Helper: stash the NativeContinuation in a GlobalRef. `None`
/// on failure (logged); callers return early.
fn cont_ref(env: &mut JNIEnv, cont: JObject, tag: &str) -> Option<jni::objects::GlobalRef> {
    match env.new_global_ref(cont) {
        Ok(g)  => Some(g),
        Err(e) => { log::warn!("{tag}: GlobalRef cont failed: {e}"); None }
    }
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeConnect<'l>(
    mut env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong, cont: JObject<'l>,
) {
    let Some(cont) = cont_ref(&mut env, cont, "nativeConnect") else { return };
    let Some(arc) = REG.lookup(handle as u64) else {
        jni_shared::spawn_with_continuation(cont,
            async { Ok::<bool, String>(false) }, jni_shared::marshal_bool);
        return;
    };
    jni_shared::spawn_with_continuation(cont,
        async move { Ok::<bool, String>(arc.connect().await.is_ok()) },
        jni_shared::marshal_bool);
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeDisconnect<'l>(
    mut env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong, cont: JObject<'l>,
) {
    let Some(cont) = cont_ref(&mut env, cont, "nativeDisconnect") else { return };
    let Some(arc) = REG.lookup(handle as u64) else {
        jni_shared::spawn_with_continuation(cont,
            async { Ok::<(), String>(()) }, jni_shared::marshal_void);
        return;
    };
    jni_shared::spawn_with_continuation(cont,
        async move { arc.disconnect().await; Ok::<(), String>(()) },
        jni_shared::marshal_void);
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeSignOut<'l>(
    mut env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong, cont: JObject<'l>,
) {
    let Some(cont) = cont_ref(&mut env, cont, "nativeSignOut") else { return };
    let Some(arc) = REG.lookup(handle as u64) else {
        jni_shared::spawn_with_continuation(cont,
            async { Ok::<(), String>(()) }, jni_shared::marshal_void);
        return;
    };
    jni_shared::spawn_with_continuation(cont,
        async move { arc.sign_out().await; Ok::<(), String>(()) },
        jni_shared::marshal_void);
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeIsConnected<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) -> jboolean {
    let ok = with_signaling(handle, |s| s.is_connected()).unwrap_or(false);
    if ok { JNI_TRUE } else { JNI_FALSE }
}

/// True while the WS run loop is actively trying to connect /
/// reconnect (rule engine spawned a loop but handshake hasn't
/// finished). Used by the UI for "Connecting…" — distinct from
/// `isConnectRequested` ("user wants connection") because that
/// can stay true while the rule engine intentionally doesn't
/// spawn the loop (e.g., backgrounded or call_active).
#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeIsAttemptingConnect<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) -> jboolean {
    let attempting = with_signaling(handle, |s| s.ws_is_attempting_connect()).unwrap_or(false);
    if attempting { JNI_TRUE } else { JNI_FALSE }
}

// ── Rule-engine input pushers ────────────────────────────────
//
// Inputs:
//   * `setForeground` — Kotlin lifecycle observer (foreground bool).
//   * `setCallActive` — Kotlin calls this around LkTunnel
//     construction / teardown. On the Rust binary this is
//     auto-wired via the `lktunnel` feature's global observer;
//     on Android the feature is off (avoids libwebrtc double-link
//     between this .so and liblktunnel.so), so Kotlin pushes it
//     explicitly.
//   * `mode` — auto-inferred from `set_incoming_handler` use.
//   * `user_disconnect` — `connect()` / `disconnect()` already
//     on the trait flip this in the right direction.

// `nativeSetCallActive` / `nativeSetServerActive` removed —
// computed by the lktunnel managers and pushed via
// activate/deactivate on the Signaling trait. Foreground is
// still pushed by the app's ProcessLifecycleOwner observer; the
// rule engine combines it with manager_active.

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeSetForeground<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong, fg: jboolean,
) {
    use lk_signaling::Signaling;
    let _g = runtime().enter();
    let _ = with_signaling(handle, |s| s.set_foreground(fg != JNI_FALSE));
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeIsAuthenticated<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) -> jboolean {
    let ok = with_signaling(handle, |s| s.is_authenticated()).unwrap_or(false);
    if ok { JNI_TRUE } else { JNI_FALSE }
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeIsConnectRequested<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) -> jboolean {
    let ok = with_signaling(handle, |s| s.is_connect_requested()).unwrap_or(false);
    if ok { JNI_TRUE } else { JNI_FALSE }
}

/// Returns the peer id as a Java String (or null if unauth'd).
/// Display-name lookups go through a separate call.
#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeWhoami<'l>(
    mut env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong, cont: JObject<'l>,
) {
    let Some(cont) = cont_ref(&mut env, cont, "nativeWhoami") else { return };
    let Some(arc) = REG.lookup(handle as u64) else {
        jni_shared::spawn_with_continuation(cont,
            async { Ok::<Option<String>, String>(None) }, jni_shared::marshal_string_opt);
        return;
    };
    jni_shared::spawn_with_continuation(cont,
        async move { Ok::<_, String>(arc.whoami().await.map(|p| p.id_str().to_string())) },
        jni_shared::marshal_string_opt);
}

/// Resolve a string (e.g. config-stored peer id) to a peer
/// handle. Returns the same string back on success (the id_str
/// round-trip), null on failure. The Kotlin side stores it
/// either way — the round-trip just validates the impl knows
/// the peer.
#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeResolvePeer<'l>(
    mut env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong, s: JString<'l>, cont: JObject<'l>,
) {
    let Some(cont) = cont_ref(&mut env, cont, "nativeResolvePeer") else { return };
    let s = jstr(&mut env, &s);
    let Some(arc) = REG.lookup(handle as u64) else {
        jni_shared::spawn_with_continuation(cont,
            async { Ok::<Option<String>, String>(None) }, jni_shared::marshal_string_opt);
        return;
    };
    jni_shared::spawn_with_continuation(cont,
        async move { Ok::<_, String>(arc.resolve_peer(&s).await.map(|p| p.id_str().to_string())) },
        jni_shared::marshal_string_opt);
}

// `nativePlaceCall` removed — outgoing calls go through
// `LkManagerNative.nativePlaceCall` on a `ClientTunnelManager`.
// `BaleSignalingNative` now only exposes auth / contacts /
// connect / disconnect.

/// Install (or clear) the incoming-call handler. `null` argument
/// removes any existing handler.
#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeSetIncomingHandler<'l>(
    mut env:  JNIEnv<'l>,
    _cls:     JClass<'l>,
    handle:   jlong,
    j_handler: JObject<'l>,
) {
    let installed: Option<Box<dyn IncomingHandler>> = if j_handler.is_null() {
        None
    } else {
        match JavaIncomingHandlerBridge::new(&mut env, j_handler) {
            Ok(b)  => Some(Box::new(b)),
            Err(e) => { log::warn!("nativeSetIncomingHandler: bridge ref failed: {e}"); None }
        }
    };
    // No tokio::spawn happens in set_incoming_handler anymore
    // (it just stores the handler), but keep the runtime guard
    // anyway — cheap, future-proof against the body changing.
    let _g = runtime().enter();
    with_signaling(handle, |sig| {
        if let Some(h) = installed {
            sig.tunnel_hooks().set_incoming_handler(h);
        }
        // No clear() API on the trait yet — leaving the previous
        // handler in place when the argument is null. Future: add
        // a clear-incoming-handler shim if a real consumer needs
        // it.
    });
}

// ─── Contacts ───────────────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeListContacts<'l>(
    mut env: JNIEnv<'l>,
    _cls:    JClass<'l>,
    handle:  jlong,
    query:   JString<'l>,
    cursor:  JString<'l>,
    limit:   jint,
    cont:    JObject<'l>,
) {
    let Some(cont_g) = cont_ref(&mut env, cont, "nativeListContacts") else { return };
    let query_s  = if query .is_null() { None } else { Some(jstr(&mut env, &query)) };
    let cursor_s = if cursor.is_null() { None } else { Some(jstr(&mut env, &cursor)) };
    let n = limit as usize;
    let Some(arc) = REG.lookup(handle as u64) else {
        jni_shared::spawn_with_continuation(cont_g,
            async { Ok::<Option<lk_signaling::ContactPage>, String>(None) },
            marshal_contact_page);
        return;
    };
    jni_shared::spawn_with_continuation(
        cont_g,
        async move {
            Ok::<_, String>(
                arc.list_contacts(query_s.as_deref(), cursor_s.as_deref(), n).await.ok()
            )
        },
        marshal_contact_page,
    );
}

/// Build a `NativeContactPage` from an `Option<ContactPage>`.
/// `None` and an empty page both produce an empty array; the
/// Kotlin wrapper handles either uniformly.
fn marshal_contact_page(env: &mut JNIEnv, page: Option<lk_signaling::ContactPage>) -> jobject {
    let Some(page) = page else { return std::ptr::null_mut(); };
    let Some(cls_ref) = NATIVE_CONTACT_PAGE_CLS.get() else {
        log::warn!("marshal_contact_page: NativeContactPage class not cached");
        return std::ptr::null_mut();
    };
    let cls = <&JClass>::from(cls_ref.as_obj());
    let str_cls = match env.find_class("java/lang/String") {
        Ok(c)  => c,
        Err(_) => return std::ptr::null_mut(),
    };
    let arr = match env.new_object_array(page.peers.len() as i32, str_cls, JObject::null()) {
        Ok(a)  => a,
        Err(_) => return std::ptr::null_mut(),
    };
    for (i, p) in page.peers.iter().enumerate() {
        if let Ok(s) = env.new_string(p.id_str()) {
            let _ = env.set_object_array_element(&arr, i as i32, s);
        }
    }
    let cursor_j = page.next_cursor.and_then(|s| env.new_string(&s).ok());
    let null_obj = JObject::null();
    let cursor_ref = cursor_j.as_ref().map(AsRef::as_ref).unwrap_or(&null_obj);
    env.new_object(
        cls,
        "([Ljava/lang/String;Ljava/lang/String;)V",
        &[JValue::Object(&arr), JValue::Object(cursor_ref)],
    ).map(|o| o.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeSearchContactByPhone<'l>(
    mut env: JNIEnv<'l>,
    _cls:    JClass<'l>,
    handle:  jlong,
    phone:   JString<'l>,
    cont:    JObject<'l>,
) {
    let Some(cont_g) = cont_ref(&mut env, cont, "nativeSearchContactByPhone") else { return };
    let phone = jstr(&mut env, &phone);
    let Some(arc) = REG.lookup(handle as u64) else {
        jni_shared::spawn_with_continuation(cont_g,
            async { Ok::<Vec<String>, String>(Vec::new()) }, marshal_string_array);
        return;
    };
    jni_shared::spawn_with_continuation(
        cont_g,
        async move {
            Ok::<_, String>(arc.search_contact_by_phone(&phone).await
                .map(|peers| peers.iter().map(|p| p.id_str().to_string()).collect())
                .unwrap_or_default())
        },
        marshal_string_array,
    );
}

/// Marshaler for `Vec<String>` → `String[]`. Empty vec becomes
/// a length-0 array, never null.
fn marshal_string_array(env: &mut JNIEnv, strings: Vec<String>) -> jobject {
    build_string_array(env, strings)
}

/// Commit a peer found via search to the contact list. On Bale
/// this is a no-op because `ImportContacts` already added the
/// peer during search; we keep the JNI export so Kotlin code
/// can read intent-explicit at the call site.
#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeAddToContacts<'l>(
    mut env: JNIEnv<'l>,
    _cls:    JClass<'l>,
    handle:  jlong,
    peer_id: JString<'l>,
    cont:    JObject<'l>,
) {
    let Some(cont_g) = cont_ref(&mut env, cont, "nativeAddToContacts") else { return };
    let pid = jstr(&mut env, &peer_id);
    let Some(arc) = REG.lookup(handle as u64) else {
        jni_shared::spawn_with_continuation(cont_g,
            async { Ok::<bool, String>(false) }, jni_shared::marshal_bool);
        return;
    };
    jni_shared::spawn_with_continuation(
        cont_g,
        async move {
            let ok = match arc.resolve_peer(&pid).await {
                Some(peer) => arc.add_to_contacts(&peer).await.is_ok(),
                None       => false,
            };
            Ok::<_, String>(ok)
        },
        jni_shared::marshal_bool,
    );
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeRemoveContact<'l>(
    mut env: JNIEnv<'l>,
    _cls:    JClass<'l>,
    handle:  jlong,
    peer_id: JString<'l>,
    cont:    JObject<'l>,
) {
    let Some(cont_g) = cont_ref(&mut env, cont, "nativeRemoveContact") else { return };
    let pid = jstr(&mut env, &peer_id);
    let Some(arc) = REG.lookup(handle as u64) else {
        jni_shared::spawn_with_continuation(cont_g,
            async { Ok::<bool, String>(false) }, jni_shared::marshal_bool);
        return;
    };
    jni_shared::spawn_with_continuation(
        cont_g,
        async move {
            let ok = match arc.resolve_peer(&pid).await {
                Some(peer) => arc.remove_contact(&peer).await.is_ok(),
                None       => false,
            };
            Ok::<_, String>(ok)
        },
        jni_shared::marshal_bool,
    );
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativeFetchDisplayName<'l>(
    mut env: JNIEnv<'l>,
    _cls:    JClass<'l>,
    handle:  jlong,
    peer_id: JString<'l>,
    cont:    JObject<'l>,
) {
    let Some(cont_g) = cont_ref(&mut env, cont, "nativeFetchDisplayName") else { return };
    let pid = jstr(&mut env, &peer_id);
    let Some(arc) = REG.lookup(handle as u64) else {
        jni_shared::spawn_with_continuation(cont_g,
            async { Ok::<Option<String>, String>(None) }, jni_shared::marshal_string_opt);
        return;
    };
    jni_shared::spawn_with_continuation(
        cont_g,
        async move {
            let name = match arc.resolve_peer(&pid).await {
                Some(peer) => arc.fetch_display_name(&peer).await,
                None       => None,
            };
            Ok::<_, String>(name)
        },
        jni_shared::marshal_string_opt,
    );
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleSignalingNative_nativePeerDisplayName<'l>(
    mut env: JNIEnv<'l>,
    _cls:    JClass<'l>,
    handle:  jlong,
    peer_id: JString<'l>,
) -> jstring {
    let pid = jstr(&mut env, &peer_id);
    let name = with_signaling(handle, |sig| {
        let sig = sig.clone();
        runtime().block_on(async move {
            sig.resolve_peer(&pid).await.and_then(|p| sig.peer_display_name(&p))
        })
    }).flatten();
    match name {
        Some(s) => jstring_new(&mut env, &s),
        None    => std::ptr::null_mut(),
    }
}

// ─── String-array helpers ───────────────────────────────────────────────

fn empty_string_array<'l>(env: &mut JNIEnv<'l>) -> jobject {
    build_string_array(env, Vec::new())
}

fn build_string_array<'l>(env: &mut JNIEnv<'l>, strings: Vec<String>) -> jobject {
    let cls = match env.find_class("java/lang/String") {
        Ok(c)  => c,
        Err(_) => return std::ptr::null_mut(),
    };
    let arr = match env.new_object_array(strings.len() as i32, cls, JObject::null()) {
        Ok(a)  => a,
        Err(_) => return std::ptr::null_mut(),
    };
    for (i, s) in strings.iter().enumerate() {
        if let Ok(j) = env.new_string(s) {
            let _ = env.set_object_array_element(&arr, i as i32, j);
        }
    }
    arr.into_raw()
}

// ─── BaleAuth ───────────────────────────────────────────────────────────

/// Shared stateless `BaleAuth` instance. The auth methods are
/// pure RPC dispatchers — no per-caller state — so one instance
/// is fine.
fn auth() -> &'static BaleAuth {
    static AUTH: once_cell::sync::OnceCell<BaleAuth> = once_cell::sync::OnceCell::new();
    AUTH.get_or_init(BaleAuth::new)
}

/// Returns a Java string `"<txhash>:<isRegistered>"` — Kotlin
/// splits it. Simpler than building a typed object for two
/// fields. Returns null on RPC failure.
#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleAuthNative_nativeStartPhoneAuth<'l>(
    mut env: JNIEnv<'l>, _cls: JClass<'l>, phone: JString<'l>, cont: JObject<'l>,
) {
    let Some(cont) = cont_ref(&mut env, cont, "nativeStartPhoneAuth") else { return };
    let phone = jstr(&mut env, &phone);
    jni_shared::spawn_with_continuation(
        cont,
        async move {
            Ok::<_, String>(
                auth().start_phone_auth(&phone).await.ok()
                    .map(|r| format!("{}:{}", r.transaction_hash,
                                     if r.is_registered { "1" } else { "0" })),
            )
        },
        jni_shared::marshal_string_opt,
    );
}

/// Returns a Java string encoding the [`AuthOutcome`] variant.
/// Shape: `"AUTH:<access_token>"` for Authenticated,
/// `"NEEDS_SIGNUP:<tx>"` for NeedsSignupName, `null` for failures.
#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleAuthNative_nativeSubmitOtp<'l>(
    mut env: JNIEnv<'l>, _cls: JClass<'l>, tx: JString<'l>, code: JString<'l>, cont: JObject<'l>,
) {
    let Some(cont) = cont_ref(&mut env, cont, "nativeSubmitOtp") else { return };
    let tx   = jstr(&mut env, &tx);
    let code = jstr(&mut env, &code);
    jni_shared::spawn_with_continuation(
        cont,
        async move {
            let encoded = auth().submit_otp(&tx, &code).await.ok().map(encode_outcome);
            Ok::<_, String>(encoded)
        },
        jni_shared::marshal_string_opt,
    );
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_bale_BaleAuthNative_nativeSubmitSignupName<'l>(
    mut env: JNIEnv<'l>, _cls: JClass<'l>, tx: JString<'l>, name: JString<'l>, cont: JObject<'l>,
) {
    let Some(cont) = cont_ref(&mut env, cont, "nativeSubmitSignupName") else { return };
    let tx   = jstr(&mut env, &tx);
    let name = jstr(&mut env, &name);
    jni_shared::spawn_with_continuation(
        cont,
        async move {
            let encoded = auth().submit_signup_name(&tx, &name).await.ok().map(encode_outcome);
            Ok::<_, String>(encoded)
        },
        jni_shared::marshal_string_opt,
    );
}

/// Encode an `AuthOutcome` into the prefixed-string wire format
/// the Kotlin side parses (avoids defining a Java result class
/// for one of two known variants).
fn encode_outcome(o: AuthOutcome) -> String {
    match o {
        AuthOutcome::Authenticated   { access_token }      => format!("AUTH:{access_token}"),
        AuthOutcome::NeedsSignupName { transaction_hash }  => format!("NEEDS_SIGNUP:{transaction_hash}"),
    }
}

