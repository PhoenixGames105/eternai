plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.eternai.core"
    compileSdk = 35

    defaultConfig {
        minSdk = 26
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    // The Rust cdylib is dropped into src/main/jniLibs/<abi>/ by cargo-ndk.
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")

    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }
}

dependencies {
    // UniFFI's generated Kotlin talks to Rust through JNA.
    api("net.java.dev.jna:jna:5.15.0@aar")
    // The generated bindings annotate the API 34 SystemCleaner path with @RequiresApi.
    implementation("androidx.annotation:annotation:1.9.1")
}
