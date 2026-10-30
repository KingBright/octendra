import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}
val signingFile = providers.environmentVariable("OCTENDRA_ANDROID_KEYSTORE").orNull
val releaseVersion = Properties().apply {
    rootProject.file("version.properties").inputStream().use { load(it) }
}
android {
    namespace = "fun.hackerlife.octendra"
    compileSdk = 36
    buildToolsVersion = "36.1.0"
    defaultConfig {
        applicationId = "fun.hackerlife.octendra"
        minSdk = 30
        targetSdk = 36
        versionCode = releaseVersion.getProperty("versionCode").toInt()
        versionName = releaseVersion.getProperty("versionName")
        buildConfigField("String", "BUILD_REVISION", "\"" + providers.environmentVariable("OCTENDRA_BUILD_REVISION").getOrElse("") + "\"")
        testInstrumentationRunner = "fun.hackerlife.octendra.AgentInstrumentation"
    }
    signingConfigs {
        if (signingFile != null) create("ownerRelease") {
            storeFile = file(signingFile)
            storePassword = providers.environmentVariable("OCTENDRA_ANDROID_STORE_PASSWORD").get()
            keyAlias = providers.environmentVariable("OCTENDRA_ANDROID_KEY_ALIAS").get()
            keyPassword = providers.environmentVariable("OCTENDRA_ANDROID_STORE_PASSWORD").get()
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
dependencies {
    testImplementation("junit:junit:4.13.2")
    // JVM tests must exercise JSON parsing instead of Android's stub methods.
    testImplementation("org.json:json:20260814")
}
kotlin { compilerOptions { jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17) } }
