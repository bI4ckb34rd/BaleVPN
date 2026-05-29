package ai.bale.proxy.tunnel

/** Snapshot of the underlying WebRTC transport. Read from the
 *  selected ICE candidate-pair and the SCTP data-channel stats;
 *  `-1` for any field the SDK hasn't reported yet (e.g. before a
 *  successful nominated pair exists). */
data class LiveKitStats(
    val rttMs:                    Long,    // -1 if unknown
    val bytesSent:                Long,    // -1 if unknown
    val bytesReceived:            Long,    // -1 if unknown
    val packetsSent:              Long,    // -1 if unknown
    val packetsReceived:          Long,    // -1 if unknown
    val availableOutgoingBitrate: Long,    // bits/sec, -1 if unknown
)
