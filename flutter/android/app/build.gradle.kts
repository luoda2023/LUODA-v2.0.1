import com.google.protobuf.gradle.*
import groovy.json.JsonSlurper
import java.util.Properties
import java.io.FileInputStream

plugins {
    id("com.google.protobuf") version "0.9.4"
    id("com.android.application")
    id("kotlin-android")
    id("dev.flutter.flutter-gradle-plugin")
}

val keystoreProperties = Properties()
val keystorePropertiesFile = rootProject.file("key.properties")
if (keystorePropertiesFile.exists()) {
    keystoreProperties.load(FileInputStream(keystorePropertiesFile))
}

val localProperties = Properties()
val localPropertiesFile = rootProject.file("local.properties")
if (localPropertiesFile.exists()) {
    localPropertiesFile.inputStream().use { reader -> localProperties.load(reader) }
}

val flutterVersionName = localProperties.getProperty("flutter.versionName") ?: "2.0"

// versionCode 由 versionName（= pubspec.yaml 的 version: X.Y.Z+N）派生，
// 保证「APK 版本号」与「主项目版本号」一一对应、单调递增，且本地构建与 CI 一致。
//
// 为什么不用 local.properties 的 flutter.versionCode / CI 的 --build-number：
// 之前 CI 传 --build-number "$GITHUB_RUN_NUMBER"，于是 versionCode 变成与版本号
// 毫无关系的流水号（实测 v2.2.37 → 954、v2.2.40 → 2957）；而本地构建没有这个参数，
// 会退回 local.properties 里的 1 —— 结果本地包 versionCode 远小于已装的 CI 包，
// adb install 直接报 INSTALL_FAILED_VERSION_DOWNGRADE。
//
// 映射：major*1000000 + minor*10000 + patch*100
//   2.2.40 → 2024000（> 已发布的 2957，升级路径不断）
//   2.2.42 → 2024200
val derivedVersionCode: Int = run {
    val parts = flutterVersionName.split(".")
    val major = parts.getOrNull(0)?.toIntOrNull() ?: 0
    val minor = parts.getOrNull(1)?.toIntOrNull() ?: 0
    val patch = parts.getOrNull(2)?.toIntOrNull() ?: 0
    major * 1_000_000 + minor * 10_000 + patch * 100
}

fun findRustlsPlatformVerifierMavenDir(): String? {
    // The rustls-platform-verifier-android crate ships a local maven repo
    // (maven/rustls/rustls-platform-verifier/...) inside its package.
    // Try common local locations first, then fall back to cargo metadata.
    val candidates = listOf(
        // unpacked crate tarball next to this project (see rustls-platform-verifier-android-0.1.1.crate)
        rootProject.file("../.rpv-maven"),
        file("../../.rpv-maven"),
    )
    for (dir in candidates) {
        if (File(dir, "rustls/rustls-platform-verifier/0.1.1").exists()) {
            println("Found rustls-platform-verifier maven repo at: ${dir.path}")
            return dir.path
        }
    }
    try {
        val dependencyText = providers.exec {
            workingDir = File("../..")
            commandLine("cargo", "metadata", "--format-version", "1")
        }.standardOutput.asText.get()

        val dependencyJson = JsonSlurper().parseText(dependencyText) as Map<*, *>
        val packages = dependencyJson["packages"] as List<*>
        val pkg = packages.find { (it as Map<*, *>)["name"] == "rustls-platform-verifier-android" }

        @Suppress("UNCHECKED_CAST")
        val pkgMap = pkg as Map<String, Any>?
        if (pkgMap == null) return null

        val manifestPath = File(pkgMap["manifest_path"] as String)
        val mavenDir = File(manifestPath.parentFile, "maven")

        if (!mavenDir.exists()) return null

        println("Found rustls-platform-verifier maven repo at: ${mavenDir.path}")
        return mavenDir.path
    } catch (e: Exception) {
        println("Warning: Could not locate rustls-platform-verifier maven repo: ${e.message}")
        return null
    }
}

val rustlsMavenDir = findRustlsPlatformVerifierMavenDir()

repositories {
    if (rustlsMavenDir != null) {
        maven {
            url = uri(rustlsMavenDir)
            metadataSources {
                mavenPom()
                artifact()
            }
        }
    }
}

tasks.register<Copy>("copyProtoFiles") {
    from(file("../../../libs/hbb_common/protos"))
    into(file("src/main/proto"))
    include("*.proto")
}

protobuf {
    protoc {
        artifact = "com.google.protobuf:protoc:3.20.1"
    }
    generateProtoTasks {
        all().forEach { task ->
            task.dependsOn("copyProtoFiles")
            task.builtins {
                create("java") {
                    option("lite")
                }
            }
        }
    }
}

android {
    namespace = "com.luoda.remote"
    compileSdkVersion(36)

    packagingOptions {
        jniLibs {
            useLegacyPackaging = true
        }
    }

    sourceSets {
        getByName("main") {
            java.srcDirs("src/main/kotlin")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
        isCoreLibraryDesugaringEnabled = true
    }

    kotlin {
        compilerOptions {
            jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
        }
    }

    defaultConfig {
        applicationId = "com.luoda.remote"
        minSdkVersion(24)
        targetSdkVersion(33)
        versionCode = derivedVersionCode
        versionName = flutterVersionName
    }

    signingConfigs {
        create("release") {
            // storeFile 路径相对于 app 目录 (即 flutter/android/app/),
            // 因为 keystore.jks 实际生成在 flutter/android/app/keystore.jks,
            // 而 rootProject 是 flutter/android/,不能直接用 rootProject.file("keystore.jks")。
            val storeFilePath = keystoreProperties["storeFile"] as? String
            val storeFileObj = storeFilePath?.let { file(it) }
            if (storeFileObj?.exists() == true) {
                keyAlias = keystoreProperties["keyAlias"] as String?
                keyPassword = keystoreProperties["keyPassword"] as String?
                storeFile = storeFileObj
                storePassword = keystoreProperties["storePassword"] as String?
            }
        }
    }

    buildTypes {
        release {
            // 与官方 rustdesk 一致：release 始终用 signingConfigs.release（而非 debug 签名）
            // 如果 key.properties 不存在或 storeFile 不存在，则为 null 签名（装不上）。
            // 见官方：https://github.com/rustdesk/rustdesk/blob/master/flutter/android/app/build.gradle
            signingConfig = signingConfigs.getByName("release")
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules"
            )
        }
    }
}

flutter {
    source = "../.."
}

dependencies {
    coreLibraryDesugaring("com.android.tools:desugar_jdk_libs:2.0.4")
    implementation("com.google.protobuf:protobuf-javalite:3.20.1")
    implementation("androidx.media:media:1.6.0")
    implementation("com.github.getActivity:XXPermissions:18.5")
    implementation("org.jetbrains.kotlin:kotlin-stdlib") {
        version { strictly("2.2.20") }
    }
    implementation("com.caverock:androidsvg-aar:1.4")
    implementation("rustls:rustls-platform-verifier:0.1.1")
}
