import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
    id("org.jetbrains.kotlin.plugin.serialization")
}

// Built by scripts/build-apk.sh before Gradle runs; never committed.
val generatedUniffi = layout.buildDirectory.dir("generated/uniffi/kotlin")
val generatedJniLibs = layout.buildDirectory.dir("generated/jniLibs")
// The host (x86_64 Linux) build of the same library, for the JVM tests that
// exercise the generated bindings.
val hostLibDir = rootDir.resolve("../target/debug")

// Sideload signing: a properties file outside the repo (storeFile,
// storePassword, keyAlias, keyPassword). scripts/build-apk.sh creates it.
val signingFile = (findProperty("livesync.signing") as String?)
    ?: System.getenv("LIVESYNC_SIGNING")
    ?: "${System.getProperty("user.home")}/.config/livesync-android/sideload.properties"
val signingProps = Properties().apply {
    val f = file(signingFile)
    if (f.exists()) f.inputStream().use { load(it) }
}

android {
    namespace = "no.nyta.livesync"
    compileSdk = 35

    defaultConfig {
        applicationId = "no.nyta.livesync"
        // Android 9 (branch android9-minsdk28, for the Onyx Boox Note 2). All-files
        // access (MANAGE_EXTERNAL_STORAGE, spec D3) exists from Android 11; below
        // it the legacy storage permissions stand in (ui/StorageAccess.kt).
        minSdk = 28
        targetSdk = 35
        versionCode = (findProperty("livesync.versionCode") as String?)?.toInt() ?: 1
        versionName = (findProperty("livesync.versionName") as String?) ?: "0.1.0-dev"
        ndk { abiFilters += listOf("arm64-v8a") }
    }

    signingConfigs {
        if (signingProps.getProperty("storeFile") != null) {
            create("sideload") {
                storeFile = file(signingProps.getProperty("storeFile"))
                storePassword = signingProps.getProperty("storePassword")
                keyAlias = signingProps.getProperty("keyAlias")
                keyPassword = signingProps.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            // No R8: JNA and the UniFFI bindings are reached by reflection, and
            // there is no device here to catch a stripped class before the user does.
            isMinifyEnabled = false
            signingConfig = signingConfigs.findByName("sideload")
        }
    }

    sourceSets {
        getByName("main") {
            java.srcDir(generatedUniffi)
            jniLibs.srcDir(generatedJniLibs)
        }
    }

    packaging {
        // Extract the .so files to nativeLibraryDir, the classic path that
        // System.loadLibrary and JNA's dlopen both search.
        jniLibs.useLegacyPackaging = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    lint {
        abortOnError = true
        warningsAsErrors = false
        checkReleaseBuilds = true
        lintConfig = file("lint.xml")
    }

    testOptions {
        unitTests.isReturnDefaultValues = true
        unitTests.all {
            it.systemProperty("jna.library.path", hostLibDir.absolutePath)
            it.systemProperty("livesync.hostLib", hostLibDir.resolve("liblivesync_ffi.so").absolutePath)
            it.systemProperty("livesync.requireHostLib", (findProperty("livesync.requireHostLib") as String?) ?: "false")
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
    }
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2025.05.00")
    implementation(composeBom)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-core")
    implementation("androidx.activity:activity-compose:1.10.1")
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.7")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.8.1")
    implementation("net.java.dev.jna:jna:5.17.0@aar")

    testImplementation("junit:junit:4.13.2")
    // The desktop JNA (with its Linux dispatch library) for the binding tests.
    testImplementation("net.java.dev.jna:jna:5.17.0")
}
