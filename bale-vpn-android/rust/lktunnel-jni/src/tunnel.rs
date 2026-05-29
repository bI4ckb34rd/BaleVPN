//! Tunnel-side JNI surface (formerly `lktunnel-android`).
//!
//! Symbols: `Java_ai_bale_proxy_{NativeJni,LkTunnelNative}_*`.
//! The JNI methods are thin delegates to [`lktunnel::LkTunnel`] —
//! connect, start_server, attach_tun, etc. No per-tunnel state
//! lives here; the JNI handle is an `Arc<LkTunnel>` in the shared
//! handle registry.
//!
//! `JNI_OnLoad` lives in the crate root (`lib.rs`); this module
//! exposes [`init_class_refs`] which the unified loader calls to
//! cache module-local class GlobalRefs.

use jni::objects::{GlobalRef, JClass, JObject, JValue};
use jni::sys::{jboolean, jint, jlong, jlongArray, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;
use once_cell::sync::OnceCell;

/// GlobalRef to `ai.bale.proxy.NativeJni`. Bound in `JNI_OnLoad`
/// (via [`init_class_refs`]), never cleared (process-lifetime).
/// The error-drain JNI fn looks `onNativeError` up by name on
/// each batch — cheap, since the path runs at most a few times
/// per second.
static NATIVE_JNI: OnceCell<GlobalRef> = OnceCell::new();

/// Called by the unified `JNI_OnLoad` to cache class GlobalRefs
/// that this module needs. Must run from the `System.loadLibrary`
/// thread (where the app classloader is current).
pub(crate) fn init_class_refs(mut env: JNIEnv<'_>) {
    if let Ok(cls) = env.find_class("ai/bale/proxy/NativeJni") {
        if let Ok(g) = env.new_global_ref(&cls) {
            let _ = NATIVE_JNI.set(g);
        }
    }
}

/// JNI handle: literally a `Box<lktunnel::LkTunnel>` cast to `jlong`.
/// All state — NAT, TUN, counters, send queue, room, lifecycle — lives
/// inside [`lktunnel::LkTunnel`]. No additional fields, no id lookups,
/// no mode enum: the LK side knows everything it needs to.

// ── JNI surface ─────────────────────────────────────────────────────

// `LkNative.nativeVersion` removed — the `.so` is loaded by
// `LkManagerNative.<clinit>` + `BaleSignalingNative.<clinit>` at
// app startup (BaleConnection.init constructs both well before
// any `LkTunnel` instance ever appears), so the redundant
// "trigger the loader" calls in LkTunnel and NativeJni are no
// longer needed.

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_NativeJni_natSetDebug<'l>(
    _env: JNIEnv<'l>,
    _cls: JClass<'l>,
    enabled: jboolean,
) {
    lktunnel::nat::set_debug(enabled != JNI_FALSE);
}

/// Drain the native error queue into `NativeJni.onNativeError`. Same
/// pull-model the C++ implementation had: producers never touch JVM,
/// the JNI cost lives here on the poller thread.
#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_NativeJni_drainNativeErrors<'l>(
    mut env: JNIEnv<'l>,
    _cls: JClass<'l>,
) {
    let cls_ref = match NATIVE_JNI.get() { Some(c) => c, None => return };
    let batch = lktunnel::errors::drain();
    for item in batch {
        let jop  = match env.new_string(&item.op)  { Ok(s) => s, Err(_) => continue };
        let jmsg = match env.new_string(&item.msg) { Ok(s) => s, Err(_) => continue };
        let res = env.call_static_method(
            <&JClass>::from(cls_ref.as_obj()),
            "onNativeError",
            "(JLjava/lang/String;ILjava/lang/String;)V",
            &[
                JValue::Long(item.sid as jlong),
                JValue::Object(&JObject::from(jop)),
                JValue::Int(item.code as jint),
                JValue::Object(&JObject::from(jmsg)),
            ],
        );
        if let Err(e) = res {
            log::warn!("drainNativeErrors: dispatch failed: {e}");
        }
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
    }
}

// Tunnel construction is no longer exposed at this JNI layer —
// tunnels are built exclusively by the lktunnel managers
// (`ClientTunnelManager` / `ServerTunnelManager` in
// `lktunnel::manager`, surfaced via `manager.rs` JNI here). The
// Kotlin `LkTunnel` is a pure-adopt wrapper over a tunnel handle
// it gets from a manager's SessionEvent.

/// Process-wide handle registry — `jlong` Kotlin handles map to
/// `Arc<LkTunnel>`. The shared [`jni_shared::HandleRegistry`]
/// gives the same per-call Arc-clone semantics + atomic
/// remove_and_take that the bale-signaling shim uses; concurrent
/// JNI calls are safe even if `nativeDisconnect` runs in
/// parallel, which is why the Kotlin side doesn't need
/// `@Synchronized` on each native call.
pub(crate) static REG: jni_shared::RegistryHandle<lktunnel::LkTunnel> =
    jni_shared::once_cell::sync::Lazy::new(jni_shared::HandleRegistry::new);

/// Borrow the `LkTunnel` behind a JNI handle for the duration of a
/// closure. Returns `None` for a zero or removed handle. The
/// closure receives a real `Arc<LkTunnel>` clone so the
/// underlying Inner stays alive even if `nativeDisconnect` runs
/// concurrently from another thread.
fn with_tunnel<F, R>(handle: jlong, f: F) -> Option<R>
where F: FnOnce(&std::sync::Arc<lktunnel::LkTunnel>) -> R
{
    let arc = REG.lookup(handle as u64)?;
    Some(f(&arc))
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkTunnelNative_nativeIsConnected<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) -> jboolean {
    let alive = with_tunnel(handle, |t| t.is_connected()).unwrap_or(false);
    if alive { JNI_TRUE } else { JNI_FALSE }
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkTunnelNative_nativeIsQuicConnected<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) -> jboolean {
    let up = with_tunnel(handle, |t| t.is_quic_connected()).unwrap_or(false);
    if up { JNI_TRUE } else { JNI_FALSE }
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkTunnelNative_nativeStartServer<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) {
    with_tunnel(handle, |t| {
        if let Err(e) = t.start_server() {
            log::warn!("nativeStartServer: {e}");
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkTunnelNative_nativeAttachTun<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong, fd: jint,
) {
    with_tunnel(handle, |t| {
        if let Err(e) = t.attach_tun(fd as i32) {
            log::warn!("nativeAttachTun: {e}");
        }
    });
}

/// Drop the TUN bridge. Idempotent. Used when the Android side
/// toggles the VPN off at runtime — the LK tunnel + any SOCKS5
/// listener stay up.
#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkTunnelNative_nativeDetachTun<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) {
    with_tunnel(handle, |t| {
        if let Err(e) = t.detach_tun() {
            log::warn!("nativeDetachTun: {e}");
        }
    });
}

/// Idempotently bring up the QUIC client connection to the peer.
/// Safe to call multiple times. Blocks on the lktunnel runtime so
/// Kotlin sees a sync return. Returns 1 on success, 0 on failure
/// (check logcat for cause).
#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkTunnelNative_nativeEnsureQuicClient<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) -> jboolean {
    let mut ok = JNI_FALSE;
    with_tunnel(handle, |t| {
        let t = t.clone();
        ok = match lktunnel::runtime().block_on(t.ensure_quic_client()) {
            Ok(())  => JNI_TRUE,
            Err(e)  => { log::warn!("nativeEnsureQuicClient: {e}"); JNI_FALSE }
        };
    });
    ok
}

/// Enable the LAN-facing SOCKS5 listener on `127.0.0.1:port`. Async on
/// the Rust side (it has to dial QUIC to the peer); we block here on
/// the lktunnel runtime so Kotlin gets a sync return.
///
/// Returns the bound port (typically == `port`, unless `port == 0` for
/// auto-assign) or `0` on failure. Errors are logged.
#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkTunnelNative_nativeEnableSocks5<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong, port: jint,
) -> jint {
    let port = if port < 0 { 0 } else { port as u16 };
    let mut result: jint = 0;
    with_tunnel(handle, |t| {
        let t = t.clone();
        result = match lktunnel::runtime().block_on(t.enable_socks5_server(port)) {
            Ok(addr) => addr.port() as jint,
            Err(e)   => { log::warn!("nativeEnableSocks5: {e}"); 0 }
        };
    });
    result
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkTunnelNative_nativeDisableSocks5<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) {
    with_tunnel(handle, |t| t.disable_socks5_server());
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkTunnelNative_nativeStats<'l>(
    env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) -> jlongArray {
    let Some(s) = with_tunnel(handle, |t| t.stats()) else { return std::ptr::null_mut() };
    let arr = match env.new_long_array(4) {
        Ok(a) => a,
        Err(_) => return std::ptr::null_mut(),
    };
    let _ = env.set_long_array_region(&arr, 0,
        &[s[0] as i64, s[1] as i64, s[2] as i64, s[3] as i64]);
    arr.into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkTunnelNative_nativeFlowStats<'l>(
    env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) -> jlongArray {
    let Some(st) = with_tunnel(handle, |t| t.flow_stats()).flatten() else {
        return std::ptr::null_mut();
    };
    let out: [i64; 20] = [
        st.tcp_flows          as i64,
        st.udp_flows          as i64,
        st.srtt_min_ms        as i64,
        st.srtt_med_ms        as i64,
        st.srtt_max_ms        as i64,
        st.rttvar_med_ms      as i64,
        st.rto_med_ms         as i64,
        st.flight_total_bytes as i64,
        st.rto_retx_total     as i64,
        st.tcp_state_counts[0] as i64, st.tcp_state_counts[1] as i64,
        st.tcp_state_counts[2] as i64, st.tcp_state_counts[3] as i64,
        st.tcp_state_counts[4] as i64, st.tcp_state_counts[5] as i64,
        st.tcp_state_counts[6] as i64, st.tcp_state_counts[7] as i64,
        st.tcp_state_counts[8] as i64, st.tcp_state_counts[9] as i64,
        st.tcp_state_counts[10] as i64,
    ];
    let arr = match env.new_long_array(20) {
        Ok(a) => a,
        Err(_) => return std::ptr::null_mut(),
    };
    let _ = env.set_long_array_region(&arr, 0, &out);
    arr.into_raw()
}

#[no_mangle]
pub extern "system" fn Java_ai_bale_proxy_LkTunnelNative_nativeDisconnect<'l>(
    _env: JNIEnv<'l>, _cls: JClass<'l>, handle: jlong,
) {
    if handle == 0 { return; }
    // Atomically pop the entry out of the registry and call
    // disconnect on the taken Arc. Single Mutex acquisition →
    // no concurrent `with_tunnel` can observe a handle that's
    // about to be removed. In-flight JNI calls that already
    // cloned the Arc keep their reference; Inner drops once
    // they release. The keeper task holds a Weak and exits on
    // the next upgrade-fail tick.
    if let Some(arc) = REG.remove_and_take(handle as u64) {
        arc.disconnect();
    }
}
