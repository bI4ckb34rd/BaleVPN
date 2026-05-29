package ai.bale.proxy

import ai.bale.proxy.bale.BaleSignaling
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.asSharedFlow

// The WS-activation gate ("ActivationDriver") lives entirely on
// the native side now — it's a process-singleton owned inside
// `lktunnel-jni` and initialised on first manager construction.
// Kotlin doesn't model it; managers don't take it as a
// constructor arg; mode swaps don't have to thread it through.

/**
 * Per-peer session lifecycle event, emitted by both manager
 * flavours. Strict pairing: exactly one [Disconnected] per
 * [Connected]. `tunnelHandle` is an opaque pointer into the
 * lktunnel handle registry — wrap it in [LkTunnel] to drive.
 */
sealed interface SessionEvent {
    data class Connected   (val peerId: String, val tunnelHandle: Long) : SessionEvent
    data class Disconnected(val peerId: String)                         : SessionEvent
    /** Tunnel never reached Connected (handshake failure, peer
     *  never joined). Distinct from [Disconnected] so consumers
     *  awaiting Connected can complete-with-error. */
    data class Failed      (val peerId: String)                         : SessionEvent
}

/** Outcome of [ClientTunnelManager.placeCall]. */
sealed interface PlaceCallResult {
    data object Ok               : PlaceCallResult
    data object Rejected         : PlaceCallResult
    data object NoPeer           : PlaceCallResult
    data object NotAuthenticated : PlaceCallResult
    data object Transport        : PlaceCallResult
}

/** What an [AdmissionDecider] returns for an incoming call. */
sealed interface CallDecision {
    data object Accept         : CallDecision
    data object Reject         : CallDecision
    data object SilentlyIgnore : CallDecision
}

/** Admission policy for [ServerTunnelManager.setAdmission]. */
fun interface AdmissionDecider {
    suspend fun decide(peerId: String, displayName: String?): CallDecision
}

private fun CallDecision.toCode(): Int = when (this) {
    CallDecision.Accept         -> 0
    CallDecision.Reject         -> 1
    CallDecision.SilentlyIgnore -> 2
}

/**
 * One outgoing call at a time. The manager owns the WS lifecycle
 * via the signaling impl's `activate` / `deactivate` — apps push
 * foreground state through [setForeground] (default true) and
 * the manager pauses the WS during a live call automatically.
 */
class ClientTunnelManager(
    signaling: BaleSignaling,
) : AutoCloseable {

    private val handle: Long = LkManagerNative.nativeClientManager(signaling.handle)
        .also { require(it != 0L) { "ClientTunnelManager: native ctor returned 0" } }

    private val _events = MutableSharedFlow<SessionEvent>(replay = 0, extraBufferCapacity = 32)
    val events: SharedFlow<SessionEvent> = _events.asSharedFlow()

    private val listener = object : LkManagerNative.SessionListener {
        override fun onConnected(peerId: String, tunnelHandle: Long) {
            val ok = _events.tryEmit(SessionEvent.Connected(peerId, tunnelHandle))
            android.util.Log.i("BaleProxy",
                "ClientMgr.listener: onConnected peerId=$peerId h=$tunnelHandle " +
                "tryEmit=$ok subs=${_events.subscriptionCount.value}")
        }
        override fun onDisconnected(peerId: String) {
            val ok = _events.tryEmit(SessionEvent.Disconnected(peerId))
            android.util.Log.i("BaleProxy",
                "ClientMgr.listener: onDisconnected peerId=$peerId tryEmit=$ok " +
                "subs=${_events.subscriptionCount.value}")
        }
        override fun onFailed(peerId: String) {
            val ok = _events.tryEmit(SessionEvent.Failed(peerId))
            android.util.Log.i("BaleProxy",
                "ClientMgr.listener: onFailed peerId=$peerId tryEmit=$ok " +
                "subs=${_events.subscriptionCount.value}")
        }
    }

    init {
        LkManagerNative.nativeClientSubscribe(handle, listener)
    }

    suspend fun placeCall(peerId: String): PlaceCallResult {
        val code: Int? = suspendCancellableCoroutine { cont ->
            LkManagerNative.nativePlaceCall(handle, peerId, NativeContinuation(cont))
        }
        return when (code ?: 4) {
            0    -> PlaceCallResult.Ok
            1    -> PlaceCallResult.Rejected
            2    -> PlaceCallResult.NoPeer
            3    -> PlaceCallResult.NotAuthenticated
            else -> PlaceCallResult.Transport
        }
    }

    override fun close() { LkManagerNative.nativeClientDestroy(handle) }
}

/**
 * Many concurrent incoming calls. Per-peer registry (one tunnel
 * per peer; kill-and-replace on supersession). The manager
 * auto-activates the signaling impl on construction and
 * deactivates on [close].
 */
class ServerTunnelManager(
    signaling: BaleSignaling,
) : AutoCloseable {

    private val handle: Long = LkManagerNative.nativeServerManager(signaling.handle)
        .also { require(it != 0L) { "ServerTunnelManager: native ctor returned 0" } }

    private val _events = MutableSharedFlow<SessionEvent>(replay = 0, extraBufferCapacity = 64)
    val events: SharedFlow<SessionEvent> = _events.asSharedFlow()

    private val listener = object : LkManagerNative.SessionListener {
        override fun onConnected(peerId: String, tunnelHandle: Long) {
            val ok = _events.tryEmit(SessionEvent.Connected(peerId, tunnelHandle))
            android.util.Log.i("BaleProxy",
                "ServerMgr.listener: onConnected peerId=$peerId h=$tunnelHandle " +
                "tryEmit=$ok subs=${_events.subscriptionCount.value}")
        }
        override fun onDisconnected(peerId: String) {
            val ok = _events.tryEmit(SessionEvent.Disconnected(peerId))
            android.util.Log.i("BaleProxy",
                "ServerMgr.listener: onDisconnected peerId=$peerId tryEmit=$ok " +
                "subs=${_events.subscriptionCount.value}")
        }
        override fun onFailed(peerId: String) {
            val ok = _events.tryEmit(SessionEvent.Failed(peerId))
            android.util.Log.i("BaleProxy",
                "ServerMgr.listener: onFailed peerId=$peerId tryEmit=$ok " +
                "subs=${_events.subscriptionCount.value}")
        }
    }

    init {
        LkManagerNative.nativeServerSubscribe(handle, listener)
    }

    /** Install / replace the admission policy. */
    fun setAdmission(decider: AdmissionDecider) {
        // Bridge suspend → sync — the JNI decide() is blocking
        // on the Rust side via spawn_blocking, so a runBlocking
        // here is safe (we're already on a worker thread).
        LkManagerNative.nativeSetAdmission(handle, object : LkManagerNative.AdmissionDecider {
            override fun decide(peerId: String, displayName: String?): Int =
                kotlinx.coroutines.runBlocking { decider.decide(peerId, displayName).toCode() }
        })
    }

    override fun close() { LkManagerNative.nativeServerDestroy(handle) }
}
