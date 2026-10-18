plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}
val signingFile = providers.environmentVariable("RH_ANDROID_KEYSTORE").orNull
android {
    namespace = "fun.hackerlife.octendra"
    compileSdk = 36
    defaultConfig {
        applicationId = "fun.hackerlife.octendra"
        minSdk = 30
        targetSdk = 36
        versionCode = 1101
        versionName = "0.11.1"
        testInstrumentationRunner = "fun.hackerlife.octendra.AgentInstrumentation"
    }
    signingConfigs {
        if (signingFile != null) create("ownerRelease") {
            storeFile = file(signingFile)
            storePassword = providers.environmentVariable("RH_ANDROID_STORE_PASSWORD").get()
            keyAlias = "remote-hosts-android"
            keyPassword = providers.environmentVariable("RH_ANDROID_STORE_PASSWORD").get()
            enableV1Signing = false
            enableV2Signing = true
            enableV3Signing = true
        }
    }
    buildTypes {
        release {
            isDebuggable = false
            isMinifyEnabled = true
            isShrinkResources = true
            signingConfig = signingConfigs.findByName("ownerRelease")
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
        debug { applicationIdSuffix = ".debug" }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures { buildConfig = true }
    lint { abortOnError = true; checkReleaseBuilds = true }
}
dependencies { testImplementation("junit:junit:4.13.2") }
kotlin { compilerOptions { jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17) } }
