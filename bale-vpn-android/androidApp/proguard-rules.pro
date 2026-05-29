# R8 / ProGuard keep rules for the release build.

# Anything with a native method — keep both the class and the
# native method signatures so JNI symbol resolution still works.
-keepclasseswithmembernames class * {
    native <methods>;
}

# Application class — referenced by name from AndroidManifest as
# `android:name=".BaleApp"`. R8 should auto-keep on manifest match
# but it's been observed to strip it; pin explicitly.
-keep class ai.bale.proxy.BaleApp { *; }

# Native JNI surfaces (Rust calls back into these by name).
-keep class ai.bale.proxy.LkNative              { *; }
-keep class ai.bale.proxy.NativeJni             { *; }
-keep class ai.bale.proxy.LkTunnel              { *; }
-keep class ai.bale.proxy.LkTunnel$*            { *; }
-keep class ai.bale.proxy.LkTunnelNative        { *; }
-keep class ai.bale.proxy.LkManagerNative       { *; }
-keep class ai.bale.proxy.LkManagerNative$*     { *; }

# Session listener / admission decider interfaces — Rust looks up
# their method IDs by name (`onConnected`, `onDisconnected`,
# `decide`); R8 would otherwise rename them.
-keep interface ai.bale.proxy.LkManagerNative$SessionListener     { *; }
-keep interface ai.bale.proxy.LkManagerNative$AdmissionDecider    { *; }

# Async-JNI continuation bridge (jni-shared `spawn_with_continuation`).
# Rust resolves `onSuccess(Object)` / `onError(String)` BY NAME on the
# passed-in instance.
-keep class ai.bale.proxy.NativeContinuation { *; }

# BaleSignaling JNI surface — Rust looks classes up via env.find_class.
-keep class ai.bale.proxy.bale.** { *; }
