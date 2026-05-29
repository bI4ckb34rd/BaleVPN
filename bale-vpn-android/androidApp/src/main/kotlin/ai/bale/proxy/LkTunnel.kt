package ai.bale.proxy

import ai.bale.proxy.tunnel.LiveKitStats

/**
 * Thin wrapper over an `lktunnel::LkTunnel` handle produced by
 * either [ClientTunnelManager] or [ServerTunnelManager]. Apps
 * never construct one directly — the manager hands a handle out
 * via [SessionEvent.Connected]; the consumer wraps it in this
 * class to drive `startServer` / `attachTun` / `enableSocks5Server`
 * etc.
 *
 * Lifecycle is observed on the *manager's* events flow, not on
 * the tunnel itself. `disconnect()` is idempotent.
 */
class LkTunnel(private val handle: Long) : AutoCloseable {

    init { require(handle != 0L) { "LkTunnel: handle == 0" } }

    @Volatile var lastStats: LiveKitStats? = null
        private set

    /** Native poll — true while the underlying engine considers
     *  the tunnel alive. */
    val isConnected: Boolean get() = LkTunnelNative.nativeIsConnected(handle)

    /** Enable server-mode NAT. The tunnel must have been produced
     *  by a [ServerTunnelManager] (which uses the server-role
     *  constructor); calling on a client-role handle is a no-op
     *  on the native side. */
    fun startServer()                         { LkTunnelNative.nativeStartServer(handle) }
    fun attachTun(fd: Int)                    { LkTunnelNative.nativeAttachTun(handle, fd) }
    fun detachTun()                           { LkTunnelNative.nativeDetachTun(handle) }
    fun ensureQuicClient(): Boolean           = LkTunnelNative.nativeEnsureQuicClient(handle)
    fun enableSocks5Server(port: Int): Int    = LkTunnelNative.nativeEnableSocks5(handle, port)
    fun isQuicConnected(): Boolean            = LkTunnelNative.nativeIsQuicConnected(handle)
    fun disableSocks5Server()                 { LkTunnelNative.nativeDisableSocks5(handle) }
    fun stats():     LongArray?               = LkTunnelNative.nativeStats(handle)
    fun flowStats(): LongArray?               = LkTunnelNative.nativeFlowStats(handle)

    /** Tear down. Idempotent. */
    fun disconnect() = close()

    @Volatile private var closed: Boolean = false
    override fun close() {
        if (closed) return
        closed = true
        LkTunnelNative.nativeDisconnect(handle)
    }

    @Suppress("ProtectedInFinal")
    protected fun finalize() { close() }

    companion object {
        // No `.so` loader priming needed — `LkManagerNative` and
        // `BaleSignalingNative` both `System.loadLibrary` in their
        // own `init {}` blocks, and `BaleConnection.init`
        // constructs both at app startup, well before any
        // `LkTunnel` is ever instantiated.

        /** Toggle verbose logging in the native NAT layer. */
        @JvmStatic
        fun setDebug(enabled: Boolean) = NativeJni.natSetDebug(enabled)
    }
}

internal object LkTunnelNative {
    @JvmStatic external fun nativeIsConnected(handle: Long): Boolean
    @JvmStatic external fun nativeIsQuicConnected(handle: Long): Boolean
    @JvmStatic external fun nativeStartServer(handle: Long)
    @JvmStatic external fun nativeAttachTun(handle: Long, fd: Int)
    @JvmStatic external fun nativeDetachTun(handle: Long)
    @JvmStatic external fun nativeEnsureQuicClient(handle: Long): Boolean
    @JvmStatic external fun nativeEnableSocks5(handle: Long, port: Int): Int
    @JvmStatic external fun nativeDisableSocks5(handle: Long)
    @JvmStatic external fun nativeStats(handle: Long): LongArray?
    @JvmStatic external fun nativeFlowStats(handle: Long): LongArray?
    @JvmStatic external fun nativeDisconnect(handle: Long)
}
