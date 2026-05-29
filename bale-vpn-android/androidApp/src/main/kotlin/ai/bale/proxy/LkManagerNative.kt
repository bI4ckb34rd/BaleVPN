package ai.bale.proxy

import ai.bale.proxy.NativeContinuation

/**
 * JNI bindings for `lktunnel::manager` — the per-peer session
 * registry + lifecycle manager that wraps a `BaleSignaling`
 * instance.
 *
 * Two flavours, both opaque jlong handles into the merged
 * `liblktunnel_jni.so`:
 *   * **Client** — one outgoing call at a time. `placeCall(peer)`
 *     dials; the resulting Connected / Disconnected lifecycle
 *     surfaces on the `SessionListener` installed via
 *     [nativeClientSubscribe].
 *   * **Server** — many concurrent incoming calls. The manager
 *     installs its own IncomingHandler on the wrapped signaling
 *     impl that delegates to the user's [AdmissionDecider]
 *     (installed via [nativeSetAdmission]); each accepted call
 *     surfaces on the listener.
 *
 * The managers auto-drive the underlying signaling impl's
 * `activate` / `deactivate` from their own state (foreground +
 * active-session-count for client, always-active for server) —
 * apps don't push `setForeground` / `setServerActive` /
 * `setCallActive` directly any more.
 */
object LkManagerNative {

    init {
        // Same merged `.so` as BaleSignalingNative.
        System.loadLibrary("lktunnel_jni")
    }

    /** Per-peer session lifecycle.
     *
     *    - `onConnected`    — handshake succeeded.
     *    - `onDisconnected` — was Connected, now torn down.
     *    - `onFailed`       — never reached Connected (handshake
     *                          failure, peer never joined). */
    interface SessionListener {
        fun onConnected   (peerId: String, tunnelHandle: Long)
        fun onDisconnected(peerId: String)
        fun onFailed      (peerId: String)
    }

    /** Admission policy for ServerTunnelManager. Return:
     *    0 = Accept, 1 = Reject, 2 = SilentlyIgnore. */
    interface AdmissionDecider {
        fun decide(peerId: String, displayName: String?): Int
    }

    // ── activation driver ─────────────────────────────────────
    //
    // The driver is a process-singleton owned by the native side,
    // initialised on the first `nativeClientManager` /
    // `nativeServerManager` call against a given BaleSignaling
    // instance. Kotlin doesn't see it.

    // ── client ────────────────────────────────────────────────

    /** Construct a ClientTunnelManager around the BaleSignaling
     *  identified by [signalingHandle]. Returns an opaque handle
     *  or 0 on failure. */
    @JvmStatic external fun nativeClientManager(signalingHandle: Long): Long

    /** Free a ClientTunnelManager. Deactivates the signaling impl
     *  (it tears the control channel down subject to its other
     *  gates). Idempotent. */
    @JvmStatic external fun nativeClientDestroy(handle: Long)

    /** Subscribe to per-peer session lifecycle. Single-listener
     *  — calling again replaces the previous one. Returns `false`
     *  if the handle is invalid. */
    @JvmStatic external fun nativeClientSubscribe(handle: Long, listener: SessionListener): Boolean

    /** Place a call. Async — completes via the continuation with
     *  an int error code (0=Ok, 1=Rejected, 2=NoPeer,
     *  3=NotAuthenticated, 4=Transport, 5=BadHandle). The
     *  Connected event for the resulting session arrives on
     *  the [SessionListener] installed by [nativeClientSubscribe]. */
    @JvmStatic external fun nativePlaceCall(
        handle: Long, peerId: String, cont: NativeContinuation<Int>,
    )

    // `nativeClientHangUp` removed — apps tear the tunnel down
    // by dropping their `LkTunnel` (which calls `disconnect()`);
    // the manager's watcher catches `EngineEvent::Disconnected`,
    // clears its internal `current` slot, and emits
    // `SessionEvent.Disconnected`. No explicit hang-up needed.

    // ── server ────────────────────────────────────────────────

    @JvmStatic external fun nativeServerManager(signalingHandle: Long): Long

    @JvmStatic external fun nativeServerDestroy(handle: Long)

    @JvmStatic external fun nativeServerSubscribe(handle: Long, listener: SessionListener): Boolean

    /** Install / replace the admission policy. Returns `false`
     *  on a bad handle. */
    @JvmStatic external fun nativeSetAdmission(handle: Long, decider: AdmissionDecider): Boolean
}
