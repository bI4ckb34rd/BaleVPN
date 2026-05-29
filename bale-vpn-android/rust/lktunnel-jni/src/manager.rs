//! JNI surface for [`lktunnel::manager`]. Exposes the two
//! managers (`ClientTunnelManager` / `ServerTunnelManager`) as
//! opaque handles, plus the `SessionEvent` Connected/Disconnected
//! stream as callbacks into a Kotlin `SessionListener`.
//!
//! Symbols: `Java_ai_bale_proxy_LkManagerNative_*`.

use crate::tunnel::REG as TUNNEL_REG;
use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::signature::{Primitive, ReturnType};
use jni::sys::{jboolean, jlong, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;
use jni_shared::{vm, runtime, HandleRegistry, RegistryHandle};
use lk_signaling::{CallDecision, IncomingHandler, PeerId, PlaceCallError};
use lktunnel::manager::{
    ActivationDriver, ClientTunnelManager, ServerTunnelManager, SessionEvent,
};
use std::sync::Arc;

// ── handle registries ──────────────────────────────────────────────────

pub(crate) static CLIENT_REG: RegistryHandle<ClientTunnelManager<bale_signaling::BaleSignaling>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);

pub(crate) static SERVER_REG: RegistryHandle<ServerTunnelManager<bale_signaling::BaleSignaling>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);

/// Process-singleton `ActivationDriver` per the single
/// `BaleSignaling` instance. The driver survives client↔server
/// manager swaps so the WS-activation gate state stays consistent
/// across mode transitions. Hidden from Kotlin: callers of
/// `nativeClientManager` / `nativeServerManager` no longer need
/// to pass a driver handle.
static PROCESS_DRIVER: once_cell::sync::OnceCell<
    Arc<ActivationDriver<bale_signaling::BaleSignaling>>
> = once_cell::sync::OnceCell::new();

fn process_driver(
    sig: &Arc<bale_signaling::BaleSignaling>,
) -> Arc<ActivationDriver<bale_signaling::BaleSignaling>> {
    PROCESS_DRIVER.get_or_init(|| {
        // First-time init — `ActivationDriver::new` spawns a
        // foreground subscriber task on the shared lktunnel
        // runtime. JNI threads aren't tokio workers so enter the
        // runtime around construction.
        let _g = runtime().enter();
        ActivationDriver::new(sig.clone())
    }).clone()
}

// ── per-listener method IDs ────────────────────────────────────────────
//
// We don't cache method IDs in a global slot because the two
// managers (Client / Server) instantiate distinct Kotlin
// anonymous inner classes — caching the first class's method ID
// and invoking it on the other class is a JNI fatal ("can't call
// X.foo on instance of Y"). Instead each subscriber resolves its
// own listener-class method IDs at subscribe time and carries
// them in its `ObserverDispatch`.

struct SessionListenerMethods {
    on_connected:    jni::objects::JMethodID,
    on_disconnected: jni::objects::JMethodID,
    on_failed:       jni::objects::JMethodID,
}
unsafe impl Send for SessionListenerMethods {}
unsafe impl Sync for SessionListenerMethods {}

pub(crate) fn init_class_refs(_env: JNIEnv<'_>) {
    // Method IDs are resolved per-subscribe inside
    // `resolve_session_listener_methods`.
}

// ── manager construction ──────────────────────────────────────────────
//
// `nativeActivationDriver` / `nativeDestroyDriver` removed — the
// driver is a hidden process-singleton fetched via
// `process_driver()` at first manager construction. Kotlin no
// longer has an `ActivationDriver` wrapper.

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkManagerNative_nativeClientManager<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, signaling_handle: jlong,
) -> jlong {
    let Some(sig) = crate::signaling::REG.lookup(signaling_handle as u64) else {
        log::warn!("nativeClientManager: bad signaling handle");
        return 0;
    };
    let drv = process_driver(&sig);
    // ClientTunnelManager::new spawns a CallEnded subscriber
    // task — needs a tokio runtime in scope or tokio::spawn
    // panics. JNI threads aren't tokio workers; enter the
    // shared runtime around construction.
    let _g = runtime().enter();
    let mgr = ClientTunnelManager::new(sig, drv);
    CLIENT_REG.insert(mgr) as jlong
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkManagerNative_nativeServerManager<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, signaling_handle: jlong,
) -> jlong {
    let Some(sig) = crate::signaling::REG.lookup(signaling_handle as u64) else {
        log::warn!("nativeServerManager: bad signaling handle");
        return 0;
    };
    let drv = process_driver(&sig);
    // ServerTunnelManager::new spawns multiple tasks (accepted
    // sessions drain, teardown watcher, CallEnded subscriber).
    // Needs a tokio runtime in scope.
    let _g = runtime().enter();
    let mgr = ServerTunnelManager::new(sig, drv);
    SERVER_REG.insert(mgr) as jlong
}

// ── subscribe ─────────────────────────────────────────────────────────

fn resolve_session_listener_methods(env: &mut JNIEnv, listener: &JObject)
    -> Option<SessionListenerMethods>
{
    let cls = env.get_object_class(listener).map_err(|e| {
        log::warn!("session listener: get_object_class: {e}");
    }).ok()?;
    let on_connected = env.get_method_id(&cls, "onConnected", "(Ljava/lang/String;J)V")
        .map_err(|e| log::warn!("session listener: onConnected mid: {e}")).ok()?;
    let on_disconnected = env.get_method_id(&cls, "onDisconnected", "(Ljava/lang/String;)V")
        .map_err(|e| log::warn!("session listener: onDisconnected mid: {e}")).ok()?;
    let on_failed = env.get_method_id(&cls, "onFailed", "(Ljava/lang/String;)V")
        .map_err(|e| log::warn!("session listener: onFailed mid: {e}")).ok()?;
    Some(SessionListenerMethods { on_connected, on_disconnected, on_failed })
}

fn fire_session_event(listener: &GlobalRef, methods: &SessionListenerMethods, ev: SessionEvent) {
    let mut env = match vm().attach_current_thread_permanently() {
        Ok(e) => e,
        Err(_) => return,
    };
    match ev {
        SessionEvent::Connected { peer_id, tunnel } => {
            let pid = match env.new_string(peer_id.id_str()) { Ok(s) => s, Err(_) => return };
            let h = TUNNEL_REG.insert(tunnel) as jlong;
            log::info!("jni session: dispatching Connected({}, h={})", peer_id.id_str(), h);
            let args = [JValue::Object(&pid).as_jni(), JValue::Long(h).as_jni()];
            unsafe {
                let _ = env.call_method_unchecked(
                    listener.as_obj(),
                    methods.on_connected,
                    ReturnType::Primitive(Primitive::Void),
                    &args,
                );
            }
        }
        SessionEvent::Disconnected { peer_id } => {
            let pid = match env.new_string(peer_id.id_str()) { Ok(s) => s, Err(_) => return };
            log::info!("jni session: dispatching Disconnected({})", peer_id.id_str());
            let args = [JValue::Object(&pid).as_jni()];
            unsafe {
                let _ = env.call_method_unchecked(
                    listener.as_obj(),
                    methods.on_disconnected,
                    ReturnType::Primitive(Primitive::Void),
                    &args,
                );
            }
        }
        SessionEvent::Failed { peer_id } => {
            let pid = match env.new_string(peer_id.id_str()) { Ok(s) => s, Err(_) => return };
            log::info!("jni session: dispatching Failed({})", peer_id.id_str());
            let args = [JValue::Object(&pid).as_jni()];
            unsafe {
                let _ = env.call_method_unchecked(
                    listener.as_obj(),
                    methods.on_failed,
                    ReturnType::Primitive(Primitive::Void),
                    &args,
                );
            }
        }
    }
    if env.exception_check().unwrap_or(false) { let _ = env.exception_clear(); }
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkManagerNative_nativeClientSubscribe<'l>(
    mut env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong, listener: JObject<'l>,
) -> jboolean {
    let Some(mgr) = CLIENT_REG.lookup(handle as u64) else {
        log::warn!("nativeClientSubscribe: bad handle"); return JNI_FALSE;
    };
    let Some(methods) = resolve_session_listener_methods(&mut env, &listener) else {
        return JNI_FALSE;
    };
    let global = match env.new_global_ref(&listener) {
        Ok(g) => g,
        Err(e) => { log::warn!("nativeClientSubscribe: NewGlobalRef: {e}"); return JNI_FALSE; }
    };
    let mut rx = mgr.events();
    runtime().spawn(async move {
        while let Some(ev) = rx.recv().await { fire_session_event(&global, &methods, ev); }
    });
    JNI_TRUE
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkManagerNative_nativeServerSubscribe<'l>(
    mut env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong, listener: JObject<'l>,
) -> jboolean {
    let Some(mgr) = SERVER_REG.lookup(handle as u64) else {
        log::warn!("nativeServerSubscribe: bad handle"); return JNI_FALSE;
    };
    let Some(methods) = resolve_session_listener_methods(&mut env, &listener) else {
        return JNI_FALSE;
    };
    let global = match env.new_global_ref(&listener) {
        Ok(g) => g,
        Err(e) => { log::warn!("nativeServerSubscribe: NewGlobalRef: {e}"); return JNI_FALSE; }
    };
    let mut rx = mgr.events();
    runtime().spawn(async move {
        while let Some(ev) = rx.recv().await { fire_session_event(&global, &methods, ev); }
    });
    JNI_TRUE
}

// ── client: place_call ────────────────────────────────────────────────
//
// Async — uses the same NativeContinuation pattern as
// BaleSignalingNative.nativePlaceCall. Returns an int error code
// via the continuation: 0=Ok, 1=Rejected, 2=NoPeer,
// 3=NotAuthenticated, 4=Transport, 5=BadHandle.

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkManagerNative_nativePlaceCall<'l>(
    mut env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong, peer_id: JString<'l>, cont: JObject<'l>,
) {
    let cont_g = match env.new_global_ref(cont) {
        Ok(g) => g,
        Err(e) => { log::warn!("nativePlaceCall: cont ref: {e}"); return; }
    };
    let peer_str: String = env.get_string(&peer_id).map(|s| s.into()).unwrap_or_default();
    let mgr = CLIENT_REG.lookup(handle as u64);
    runtime().spawn(async move {
        use lk_signaling::Signaling;
        let code: i32 = match mgr {
            None => 5,    // BadHandle
            Some(mgr) => {
                let sig = mgr.signaling();
                match sig.resolve_peer(&peer_str).await {
                    None => 2,    // NoPeer
                    Some(peer) => match mgr.place_call(peer).await {
                        Ok(())                                 => 0,
                        Err(PlaceCallError::Rejected)          => 1,
                        Err(PlaceCallError::NoPeer)            => 2,
                        Err(PlaceCallError::NotAuthenticated)  => 3,
                        Err(PlaceCallError::Transport(_))      => 4,
                    },
                }
            }
        };
        complete_int(cont_g, code);
    });
}

fn complete_int(cont: GlobalRef, code: i32) {
    // NativeContinuation only exposes `onSuccess(Any?)` /
    // `onError(String)` — there is no `onSuccessInt(int)`. Box
    // the code as `java.lang.Integer` and dispatch through the
    // generic `onSuccess` slot; Kotlin's `cont.resume(result as
    // Int)` unboxes automatically.
    let mut env = match vm().attach_current_thread_permanently() {
        Ok(e) => e,
        Err(_) => return,
    };
    let boxed = (|| -> Option<jni::objects::JObject<'_>> {
        let cls = env.find_class("java/lang/Integer").ok()?;
        let v = env.call_static_method(
            cls,
            "valueOf",
            "(I)Ljava/lang/Integer;",
            &[JValue::Int(code)],
        ).ok()?;
        v.l().ok()
    })();
    let obj = match boxed {
        Some(o) => o,
        None    => {
            log::warn!("complete_int: failed to box {code} as Integer");
            return;
        }
    };
    if let Err(e) = env.call_method(
        cont.as_obj(),
        "onSuccess",
        "(Ljava/lang/Object;)V",
        &[JValue::Object(&obj)],
    ) {
        log::warn!("complete_int: onSuccess dispatch: {e}");
    }
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
}

// ── server: admission ─────────────────────────────────────────────────

struct JavaAdmissionDecider {
    global:   GlobalRef,
    decide:   jni::objects::JMethodID,
}

// SAFETY: JMethodID is a raw pointer-like handle from the JVM;
// it's stable for the lifetime of the loaded class and we
// invoke it only via the JVM's CallMethod which is thread-safe.
unsafe impl Send for JavaAdmissionDecider {}
unsafe impl Sync for JavaAdmissionDecider {}

#[async_trait::async_trait]
impl IncomingHandler for JavaAdmissionDecider {
    async fn decide(&self, peer: PeerId, display_name: Option<String>) -> CallDecision {
        let global = self.global.clone();
        let decide = self.decide;
        let peer_str = peer.id_str().to_string();
        let r = tokio::task::spawn_blocking(move || {
            let mut env = match vm().attach_current_thread_permanently() {
                Ok(e) => e,
                Err(_) => return 2i32,
            };
            let pid = match env.new_string(&peer_str) { Ok(s) => s, Err(_) => return 2 };
            let dn  = display_name.as_deref().and_then(|s| env.new_string(s).ok());
            let null = JObject::null();
            let dn_ref = dn.as_ref().map(AsRef::as_ref).unwrap_or(&null);
            let args = [JValue::Object(&pid).as_jni(), JValue::Object(dn_ref).as_jni()];
            let r = unsafe {
                env.call_method_unchecked(
                    global.as_obj(),
                    decide,
                    ReturnType::Primitive(Primitive::Int),
                    &args,
                )
            };
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
                return 2;
            }
            r.ok().and_then(|v| v.i().ok()).unwrap_or(2)
        }).await.unwrap_or(2);
        match r {
            0 => CallDecision::Accept,
            1 => CallDecision::Reject,
            _ => CallDecision::SilentlyIgnore,
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkManagerNative_nativeSetAdmission<'l>(
    mut env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong, decider: JObject<'l>,
) -> jboolean {
    let Some(mgr) = SERVER_REG.lookup(handle as u64) else {
        log::warn!("nativeSetAdmission: bad handle"); return JNI_FALSE;
    };
    // Resolve the decide() method ID against this decider's
    // concrete class — different deciders may be different
    // anonymous classes, so a process-wide cache would be wrong.
    let cls = match env.get_object_class(&decider) {
        Ok(c) => c,
        Err(e) => { log::warn!("nativeSetAdmission: get_object_class: {e}"); return JNI_FALSE; }
    };
    let decide = match env.get_method_id(&cls, "decide", "(Ljava/lang/String;Ljava/lang/String;)I") {
        Ok(m) => m,
        Err(e) => { log::warn!("nativeSetAdmission: decide mid: {e}"); return JNI_FALSE; }
    };
    let global = match env.new_global_ref(&decider) {
        Ok(g) => g,
        Err(e) => { log::warn!("nativeSetAdmission: NewGlobalRef: {e}"); return JNI_FALSE; }
    };
    mgr.set_admission(Arc::new(JavaAdmissionDecider { global, decide }));
    JNI_TRUE
}

// ── manager destruction ───────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkManagerNative_nativeClientDestroy<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) {
    let _ = CLIENT_REG.remove_and_take(handle as u64);
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkManagerNative_nativeServerDestroy<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) {
    let _ = SERVER_REG.remove_and_take(handle as u64);
}

// `nativeClientHangUp` removed — apps drop their LkTunnel
// reference (which calls `tunnel.disconnect()`); the manager's
// client watcher sees `EngineEvent::Disconnected`, clears its
// `current` slot, and emits `SessionEvent::Disconnected`. No
// explicit hang-up call is needed.
