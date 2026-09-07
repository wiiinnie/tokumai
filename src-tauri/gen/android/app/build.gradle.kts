import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

// Release signing: the keystore lives OUTSIDE the repo (~/.tokumai-android/, created once
// with keytool). Without the properties file the release APK is built unsigned (not
// installable) — the build says so instead of failing, so CI/checks still run.
//
// The pre-rebrand location is still read as a fallback, so a machine that has not moved
// its keystore keeps building. Note that the two keystores are DIFFERENT app identities:
// an APK signed with one cannot update an install of the other. That break was accepted
// deliberately (2026-09-07) while distribution is still a handful of sideloaded APKs —
// after Play publication the key can never change again.
val keystoreProperties = Properties().apply {
    val home = System.getProperty("user.home")
    val path = System.getenv("TOKUMAI_ANDROID_KEYSTORE_PROPS")
        ?: System.getenv("SCRAI_ANDROID_KEYSTORE_PROPS")
        ?: listOf("$home/.tokumai-android/keystore.properties", "$home/.scrai-android/keystore.properties")
            .firstOrNull { file(it).exists() }
        ?: "$home/.tokumai-android/keystore.properties"
    val propFile = file(path)
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
        logger.lifecycle("tokumai: signing with the keystore named in $path")
    } else {
        logger.warn("tokumai: no keystore.properties at $path — release APK will be UNSIGNED")
    }
}

android {
    compileSdk = 36
    namespace = "com.tokumai.app"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "com.tokumai.app"
        minSdk = 24
        targetSdk = 36
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    signingConfigs {
        create("release") {
            if (keystoreProperties.containsKey("storeFile")) {
                // Trailing whitespace in a .properties value survives Properties.load (only
                // LEADING space is stripped), and the failure it produces names a path that
                // looks perfectly correct — the spaces are invisible in the message. Trim
                // everything rather than make anyone find that twice.
                fun prop(k: String) = (keystoreProperties[k] as String).trim()
                storeFile = file(prop("storeFile"))
                storePassword = prop("storePassword")
                keyAlias = prop("keyAlias")
                keyPassword = prop("keyPassword")
            }
        }
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            if (keystoreProperties.containsKey("storeFile")) {
                signingConfig = signingConfigs.getByName("release")
            }
            isMinifyEnabled = true
            proguardFiles(
                *fileTree(".") { include("**/*.pro") }
                    .plus(getDefaultProguardFile("proguard-android-optimize.txt"))
                    .toList().toTypedArray()
            )
        }
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
    buildFeatures {
        buildConfig = true
    }
}

rust {
    rootDirRel = "../../../"
}

dependencies {
    // Kotlin half of rustls-platform-verifier (TLS via the Android trust store). OUR build of
    // it: upstream 0.1.1 marks every Let's Encrypt certificate "Revoked" on Android because LE
    // dropped OCSP URLs in 2025 (rustls/rustls-platform-verifier#221) — libs/ carries the
    // patched .aar (source + build notes: docs/android.md). Replace when upstream fixes #221.
    implementation(files("libs/rustls-platform-verifier-0.1.1-scrai.aar"))
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = "tauri.build.gradle.kts")