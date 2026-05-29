package ai.bale.proxy.bale

import ai.bale.proxy.NativeContinuation

/**
 * JNI binding for the `bale-signaling` Rust crate. Loads
 * `libbale_signaling.so`, declares the `external` entry points
 * that the JNI library exports, and defines the small set of
 * Java interfaces the Rust side calls back into.
 *
 * Naming convention is matched against the Rust file's
 * `Java_ai_bale_proxy_bale_BaleSignalingNative_*` exports —
 * don't move this object out of `ai.bale.proxy.bale` without
 * updating both sides.
 *
 * Public because the nested `JavaTokenStore` / `SignalingObserver` /
 * `JavaIncomingHandler` interfaces leak through the public
 * [BaleSignaling] constructor — the wrapper is the supported
 * surface, but Kotlin can't hide the interface types if the
 * wrapper takes one as a parameter.
 */
object BaleSignalingNative {

    init {
        // Merged JNI shim — signaling, auth and lktunnel JNI all
        // live in `liblktunnel_jni.so`. Loading it here is
        // idempotent with `LkManagerNative`'s loader; whichever
        // class the app touches first triggers `JNI_OnLoad`.
        System.loadLibrary("lktunnel_jni")
    }

    /** Pluggable token storage. Kotlin implementations sit on
     *  top of SharedPreferences / EncryptedSharedPreferences /
     *  whatever the app picks. */
    interface JavaTokenStore {
        fun load (): ByteArray?
        fun save (bytes: ByteArray)
        fun clear()
    }

    /** Sink for every event the Rust side surfaces. The Kotlin
     *  wrapper translates these into Kotlin Flows / suspend
     *  callbacks so consumers don't see the JNI shape.
     *
     *  Note: there's no `onSessionReady` here. Accepted incoming
     *  sessions are surfaced via [SessionListener] (subscribed
     *  through [nativeSubscribeSessions]) — the Rust library
     *  builds the LkTunnel itself and hands Kotlin a handle. */
    interface SignalingObserver {
        fun onAuthExpired      ()
        fun onProtocolObsolete ()
        /** Fires when any call this signaling instance was
         *  tracking ends — outgoing or incoming. Apps
         *  discriminate by checking their own per-peer state.
         *  Encoded as: 0=Rejected, 1=CallerHangup, 2=Timeout,
         *  3=NetworkDrop, 100+ = Other(code-100). Kotlin maps
         *  back to [EndReason]. */
        fun onCallEnded        (peerId: String, endReasonCode: Int)
        fun onContactsChanged  ()
    }

    // ── lifecycle ─────────────────────────────────────────────

    @JvmStatic external fun nativeCreate(
        tokenStore: JavaTokenStore,
        observer:   SignalingObserver,
    ): Long
    @JvmStatic external fun nativeDestroy(handle: Long)

    // Async functions all take a `NativeContinuation` as the
    // last parameter — native spawns the work on the shared
    // tokio runtime, returns immediately, and invokes
    // `cont.onSuccess(result)` / `cont.onError(msg)` when done.
    // Kotlin wraps each in `suspendCancellableCoroutine` so
    // callers see a normal suspend signature.

    @JvmStatic external fun nativeConnect       (handle: Long, cont: NativeContinuation<Boolean>)
    @JvmStatic external fun nativeDisconnect    (handle: Long, cont: NativeContinuation<Unit?>)
    @JvmStatic external fun nativeSignOut       (handle: Long, cont: NativeContinuation<Unit?>)

    /** App lifecycle hint. Pushed by `BaleApp`'s
     *  `ProcessLifecycleOwner` observer; the rule engine
     *  combines it with `manager_active`. setCallActive /
     *  setServerActive are gone — those flags are managed by
     *  the lktunnel managers via activate/deactivate. */
    @JvmStatic external fun nativeSetForeground(handle: Long, fg: Boolean)

    // Sync polling — flag reads, no async work, kept on the
    // calling thread.
    @JvmStatic external fun nativeIsConnected   (handle: Long): Boolean
    /** True while the run loop is actively trying to connect /
     *  reconnect — UI's "Connecting…" indicator. Distinct from
     *  `isConnectRequested` (which is intent, not actual state). */
    @JvmStatic external fun nativeIsAttemptingConnect(handle: Long): Boolean
    @JvmStatic external fun nativeIsAuthenticated(handle: Long): Boolean
    /** True between `nativeConnect` and `nativeDisconnect` —
     *  API-level intent, not wire-level readiness. Reconcile
     *  uses this to avoid re-calling connect() while the run
     *  loop is mid-reconnect (`isConnected` would be false
     *  during backoff). */
    @JvmStatic external fun nativeIsConnectRequested(handle: Long): Boolean

    // ── identity ──────────────────────────────────────────────

    @JvmStatic external fun nativeWhoami      (handle: Long, cont: NativeContinuation<String?>)
    @JvmStatic external fun nativeResolvePeer (handle: Long, s: String, cont: NativeContinuation<String?>)
    @JvmStatic external fun nativeFetchDisplayName(handle: Long, peerId: String, cont: NativeContinuation<String?>)
    /** Sync: returns the cached display name or null. No RPC.
     *  Use [nativeFetchDisplayName] for a first-time lookup. */
    @JvmStatic external fun nativePeerDisplayName (handle: Long, peerId: String): String?

    // Client placeCall + server admission moved to
    // `LkManagerNative` — see `ClientTunnelManager` /
    // `ServerTunnelManager`.

    // ── contacts ──────────────────────────────────────────────

    @JvmStatic external fun nativeListContacts(
        handle: Long, query: String?, cursor: String?, limit: Int,
        cont: NativeContinuation<NativeContactPage?>,
    )
    /** Phone lookup. Note: Bale's underlying `ImportContacts`
     *  adds the match as a side effect of search — search is
     *  not non-mutating. [nativeAddToContacts] is a no-op on
     *  Bale; kept so Kotlin callers can be intent-explicit. */
    @JvmStatic external fun nativeSearchContactByPhone(handle: Long, phone: String, cont: NativeContinuation<Array<String>>)
    @JvmStatic external fun nativeAddToContacts       (handle: Long, peerId: String, cont: NativeContinuation<Boolean>)
    @JvmStatic external fun nativeRemoveContact       (handle: Long, peerId: String, cont: NativeContinuation<Boolean>)
}

// ── Top-level data classes returned by JNI ───────────────────

/** Page of contact peer ids; `nextCursor=null` means the list
 *  is exhausted. The cursor is opaque — pass it back into the
 *  next call verbatim. */
data class NativeContactPage(
    val peerIds:    Array<String>,
    val nextCursor: String?,
) {
    override fun equals(other: Any?): Boolean {
        if (this === other)              return true
        if (other !is NativeContactPage) return false
        return peerIds.contentEquals(other.peerIds) && nextCursor == other.nextCursor
    }
    override fun hashCode(): Int =
        31 * peerIds.contentHashCode() + (nextCursor?.hashCode() ?: 0)
}
