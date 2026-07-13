// Standard Tauri v2 Android plugin module — mirrors the shape `tauri plugin android init`
// generates (best-effort reconstruction; this host has no Android NDK/SDK `ndk` component to
// actually run that scaffolding command and verify the output byte-for-byte, same HOST-GATED gap
// P3's own Android bring-up steps documented).
plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

val taskName = if (gradle.startParameter.taskNames.isNotEmpty()) gradle.startParameter.taskNames[0] else ""
val hostBuild = taskName.startsWith(":tauri-android:")

android {
    namespace = "io.haily.voice"
    compileSdk = 34

    defaultConfig {
        // AudioFocusRequest (used by AudioFocus.kt for AUDIOFOCUS_GAIN_TRANSIENT_EXCLUSIVE /
        // AUDIOFOCUS_GAIN_TRANSIENT with a builder-based listener) is API 26+ — raises this
        // plugin's effective floor above the app's likely default of 24; see the phase's
        // Deviation Log for why the modern API was chosen over a legacy int-based fallback.
        minSdk = 26
        targetSdk = 34
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }

    kotlinOptions {
        jvmTarget = "1.8"
    }
}

dependencies {
    if (hostBuild) {
        implementation(project(":tauri-android"))
    } else {
        implementation("app.tauri:tauri-android:2.0.0-beta")
    }
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")
}
