package com.luoda.remote

import android.Manifest.permission.*
import android.annotation.SuppressLint
import android.content.Context
import android.content.ComponentName
import android.content.Intent
import android.media.AudioRecord
import android.media.AudioRecord.READ_BLOCKING
import android.media.MediaCodecList
import android.media.MediaFormat
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import android.provider.Settings.*
import android.util.DisplayMetrics
import android.util.Log
import android.view.WindowManager
import androidx.annotation.RequiresApi
import androidx.core.content.ContextCompat.getSystemService
import com.hjq.permissions.Permission
import com.hjq.permissions.XXPermissions
import ffi.FFI
import java.io.File
import java.nio.ByteBuffer
import java.util.*


// intent action, extra
const val ACT_REQUEST_MEDIA_PROJECTION = "REQUEST_MEDIA_PROJECTION"
const val ACT_INIT_MEDIA_PROJECTION_AND_SERVICE = "INIT_MEDIA_PROJECTION_AND_SERVICE"
const val ACT_LOGIN_REQ_NOTIFY = "LOGIN_REQ_NOTIFY"
const val EXT_INIT_FROM_BOOT = "EXT_INIT_FROM_BOOT"
const val EXT_MEDIA_PROJECTION_RES_INTENT = "MEDIA_PROJECTION_RES_INTENT"
const val EXT_LOGIN_REQ_NOTIFY = "LOGIN_REQ_NOTIFY"
const val ACT_START_CAPTURE = "START_CAPTURE"
const val EXT_START_CAPTURE_AFTER_PROJECTION = "START_CAPTURE_AFTER_PROJECTION"

// Activity requestCode
const val REQ_INVOKE_PERMISSION_ACTIVITY_MEDIA_PROJECTION = 101
const val REQ_REQUEST_MEDIA_PROJECTION = 201

// Activity responseCode
const val RES_FAILED = -100

// Flutter channel
const val START_ACTION = "start_action"
const val GET_START_ON_BOOT_OPT = "get_start_on_boot_opt"
const val SET_START_ON_BOOT_OPT = "set_start_on_boot_opt"
const val SYNC_APP_DIR_CONFIG_PATH = "sync_app_dir"
const val GET_VALUE = "get_value"

const val KEY_IS_SUPPORT_VOICE_CALL = "KEY_IS_SUPPORT_VOICE_CALL"

const val KEY_SHARED_PREFERENCES = "KEY_SHARED_PREFERENCES"
const val KEY_START_ON_BOOT_OPT = "KEY_START_ON_BOOT_OPT"
const val KEY_APP_DIR_CONFIG_PATH = "KEY_APP_DIR_CONFIG_PATH"
const val KEY_FIRST_RUN_AUTHORIZATION = "KEY_FIRST_RUN_AUTHORIZATION"
// 注意：曾经这里还有 KEY_MEDIA_PROJECTION_TOKEN（试图把投屏授权落盘以便跨进程复用）。
// MediaProjection 的结果 Intent 里没有可用的 data Uri、令牌是 IBinder，无法持久化，
// 该机制从未生效，已删除。详见 MainService.invalidateProjection() 下方的说明。

@SuppressLint("ConstantLocale")
val LOCAL_NAME = Locale.getDefault().toString()
val SCREEN_INFO = Info(0, 0, 1, 200)

data class Info(
    var width: Int, var height: Int, var scale: Int, var dpi: Int
)


/**
 * True *shared/public* external storage directory (/storage/emulated/0/Documents
 * or Download on API 28+; the root /storage/emulated/0 on older releases).
 * Unlike path_provider's app-scoped getExternalFilesDirs this path survives an
 * app uninstall/reinstall, so it is the right home for the authorization marker.
 * Prefer a user-visible folder that is guaranteed writable without extra
 * runtime permission:
 *  - API 29+: /storage/emulated/0/Documents/LDesk (no permission needed to write
 *    our own subfolder; media permissions govern other apps' files only).
 *  - API 28-: /storage/emulated/0 (app-specific subfolders there are the only
 *    reliably writable public location without runtime grants).
 */
fun publicAuthBaseDir(context: Context): String {
    return try {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            val documents = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOCUMENTS)
            File(documents, "LDesk").absolutePath
        } else {
            Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOWNLOADS).absolutePath
        }
    } catch (e: Exception) {
        Log.e("common", "publicAuthBaseDir failed: ${e.message}")
        Environment.getExternalStorageDirectory().absolutePath
    }
}

fun isSupportVoiceCall(): Boolean {
    // https://developer.android.com/reference/android/media/MediaRecorder.AudioSource#VOICE_COMMUNICATION
    return Build.VERSION.SDK_INT >= Build.VERSION_CODES.R
}

fun requestPermission(context: Context, type: String) {
    XXPermissions.with(context)
        .permission(type)
        .request { _, all ->
            Handler(Looper.getMainLooper()).post {
                MainActivity.flutterMethodChannel?.invokeMethod(
                    "on_android_permission_result",
                    mapOf("type" to type, "result" to all)
                )
            }
        }
}

/**
 * Batch-request multiple standard runtime permissions in a single system dialog.
 * Returns the per-type results via [on_android_permission_result] callback.
 */
fun requestPermissionsBatch(context: Context, types: List<String>) {
    XXPermissions.with(context)
        .permission(types)
        .request { _, all ->
            Handler(Looper.getMainLooper()).post {
                // Report each type individually so the Dart side Completer can resolve
                for (type in types) {
                    // XXPermissions returns a single `all` flag; if all granted, each is granted.
                    // If not all granted, we check individually.
                    val granted = all || XXPermissions.isGranted(context, type)
                    MainActivity.flutterMethodChannel?.invokeMethod(
                        "on_android_permission_result",
                        mapOf("type" to type, "result" to granted)
                    )
                }
            }
        }
}

fun startAction(context: Context, action: String): Boolean {
    try {
        val intent = Intent(action).apply {
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        when (action) {
            // don't pass package name when launching the accessibility list
            ACTION_ACCESSIBILITY_SETTINGS -> { /* no extras */ }
            "android.settings.ACCESSIBILITY_DETAILS_SETTINGS" -> {
                // Land on the LUODA Input toggle directly where the ROM
                // supports component-targeted accessibility settings.
                intent.putExtra(
                    Intent.EXTRA_COMPONENT_NAME,
                    ComponentName(context, InputService::class.java)
                )
            }
            else -> intent.data = Uri.parse("package:" + context.packageName)
        }
        context.startActivity(intent)
        return true
    } catch (e: Exception) {
        Log.e("common", "Unable to open Android settings action $action", e)
        if ("android.settings.ACCESSIBILITY_DETAILS_SETTINGS" == action) {
            return try {
                context.startActivity(Intent(ACTION_ACCESSIBILITY_SETTINGS).apply {
                    addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                })
                true
            } catch (fallbackError: Exception) {
                Log.e("common", "Unable to open accessibility settings", fallbackError)
                false
            }
        }
        return false
    }
}

class AudioReader(val bufSize: Int, private val maxFrames: Int) {
    private var currentPos = 0
    private val bufferPool: Array<ByteBuffer>

    init {
        if (maxFrames < 0 || maxFrames > 32) {
            throw Exception("Out of bounds")
        }
        if (bufSize <= 0) {
            throw Exception("Wrong bufSize")
        }
        bufferPool = Array(maxFrames) {
            ByteBuffer.allocateDirect(bufSize)
        }
    }

    private fun next() {
        currentPos++
        if (currentPos >= maxFrames) {
            currentPos = 0
        }
    }

    @RequiresApi(Build.VERSION_CODES.M)
    fun readSync(audioRecord: AudioRecord): ByteBuffer? {
        val buffer = bufferPool[currentPos]
        val res = audioRecord.read(buffer, bufSize, READ_BLOCKING)
        return if (res > 0) {
            next()
            buffer
        } else {
            null
        }
    }
}


fun getScreenSize(windowManager: WindowManager) : Pair<Int, Int>{
    var w = 0
    var h = 0
    @Suppress("DEPRECATION")
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
        val m = windowManager.maximumWindowMetrics
        w = m.bounds.width()
        h = m.bounds.height()
    } else {
        val dm = DisplayMetrics()
        windowManager.defaultDisplay.getRealMetrics(dm)
        w = dm.widthPixels
        h = dm.heightPixels
    }
    return Pair(w, h)
}

 fun translate(input: String): String {
    Log.d("common", "translate:$LOCAL_NAME")
    return FFI.translateLocale(LOCAL_NAME, input)
}
