package com.luoda.remote

import ffi.FFI

/**
 * Capture screen,get video and audio,send to rust.
 * Dispatch notifications
 *
 * Inspired by [droidVNC-NG] https://github.com/bk138/droidVNC-NG
 */

import android.Manifest
import android.annotation.SuppressLint
import android.app.*
import android.app.PendingIntent.FLAG_IMMUTABLE
import android.app.PendingIntent.FLAG_UPDATE_CURRENT
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.content.res.Configuration.ORIENTATION_LANDSCAPE
import android.graphics.Color
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR
import android.hardware.display.VirtualDisplay
import android.media.*
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.net.wifi.WifiManager
import android.os.*
import android.util.DisplayMetrics
import android.util.Log
import android.view.Surface
import android.view.Surface.FRAME_RATE_COMPATIBILITY_DEFAULT
import android.view.WindowManager
import androidx.annotation.Keep
import androidx.annotation.RequiresApi
import androidx.core.app.ActivityCompat
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import io.flutter.embedding.android.FlutterActivity
import java.util.concurrent.Executors
import kotlin.concurrent.thread
import org.json.JSONException
import org.json.JSONObject
import java.nio.ByteBuffer
import kotlin.math.max
import kotlin.math.min

const val DEFAULT_NOTIFY_TITLE = "LDesk"
const val DEFAULT_NOTIFY_TEXT = "Service is running"
const val DEFAULT_NOTIFY_ID = 1
const val NOTIFY_ID_OFFSET = 100

/// Minimum gap between two "remote input dropped" warnings, in milliseconds.
const val INPUT_DROP_LOG_INTERVAL_MS = 5000L

/// Dedicated notification id for "remote input cannot be injected".
const val INPUT_NOTIFY_ID = 2

const val MIME_TYPE = MediaFormat.MIMETYPE_VIDEO_VP9

// video const

const val MAX_SCREEN_SIZE = 1200

const val VIDEO_KEY_BIT_RATE = 1024_000
const val VIDEO_KEY_FRAME_RATE = 30

class MainService : Service() {

    // Throttle for the "input dropped" warning so a controller that keeps
    // moving the mouse cannot flood logcat.
    private var inputDropLoggedAt = 0L
    // One "input unavailable" notice per outage; reset as soon as the
    // accessibility service is back, so a later loss is reported again.
    private var inputUnavailableNotified = false

    /**
     * Remote input can only be injected through the accessibility service
     * (`InputService`). When the user never granted it - or the ROM dropped the
     * grant after an app update / force-stop, which happens on MIUI / EMUI /
     * ColorOS - `InputService.ctx` is null.
     *
     * This used to be swallowed by `?.` with no log and no feedback anywhere:
     * the controller saw a live picture and a dead screen, and the phone said
     * nothing. Log it (throttled) and tell the app once per outage so it can
     * offer the one-tap way back to the system toggle.
     */
    private fun inputServiceOrNull(): InputService? {
        val ctx = InputService.ctx
        if (ctx != null) {
            if (inputUnavailableNotified) {
                inputUnavailableNotified = false
                if (::notificationManager.isInitialized) {
                    notificationManager.cancel(INPUT_NOTIFY_ID)
                }
            }
            return ctx
        }
        val now = SystemClock.elapsedRealtime()
        if (now - inputDropLoggedAt > INPUT_DROP_LOG_INTERVAL_MS) {
            inputDropLoggedAt = now
            Log.w(
                logTag,
                "remote input dropped: accessibility service is not enabled " +
                    "(Settings > Accessibility > LDesk Input)"
            )
        }
        if (!inputUnavailableNotified) {
            inputUnavailableNotified = true
            Handler(Looper.getMainLooper()).post {
                MainActivity.flutterMethodChannel?.invokeMethod(
                    "on_input_unavailable", emptyMap<String, String>()
                )
            }
            notifyInputServiceMissing()
        }
        return null
    }

    /**
     * A remote session is running, the controller keeps sending input, and we
     * are dropping all of it. The app is almost always in the background while
     * being controlled, so an in-app dialog would never be seen - post a
     * dedicated notification whose tap target is the accessibility list, where
     * "LDesk Input" can be switched back on in one tap.
     *
     * Uses a fresh builder on purpose: [notificationBuilder] carries the
     * foreground-service state and must not be polluted by this one-off.
     */
    private fun notifyInputServiceMissing() {
        if (!::notificationManager.isInitialized) {
            return
        }
        try {
            val intent = Intent("android.settings.ACCESSIBILITY_SETTINGS").apply {
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            }
            val pendingIntent = PendingIntent.getActivity(
                this, 0, intent, FLAG_UPDATE_CURRENT or FLAG_IMMUTABLE
            )
            val text = translate("android_input_permission_tip1")
            val notification = NotificationCompat
                .Builder(this, notificationChannel)
                .setOngoing(false)
                .setAutoCancel(true)
                .setSmallIcon(R.mipmap.ic_stat_logo)
                .setPriority(NotificationCompat.PRIORITY_HIGH)
                .setContentTitle(DEFAULT_NOTIFY_TITLE)
                .setContentText(text)
                .setStyle(NotificationCompat.BigTextStyle().bigText(text))
                .setContentIntent(pendingIntent)
                .setColor(ContextCompat.getColor(this, R.color.primary))
                .setWhen(System.currentTimeMillis())
                .build()
            notificationManager.notify(INPUT_NOTIFY_ID, notification)
        } catch (e: Exception) {
            Log.e(logTag, "failed to post input-service notification: ${e.message}")
        }
    }

    @Keep
    @RequiresApi(Build.VERSION_CODES.N)
    fun rustPointerInput(kind: Int, mask: Int, x: Int, y: Int) {
        // turn on screen with LEFT_DOWN when screen off
        if (!powerManager.isInteractive && (kind == 0 || mask == LEFT_DOWN)) {
            if (wakeLock.isHeld) {
                Log.d(logTag, "Turn on Screen, WakeLock release")
                wakeLock.release()
            }
            Log.d(logTag,"Turn on Screen")
            wakeLock.acquire(5000)
        } else {
            when (kind) {
                0 -> { // touch
                    inputServiceOrNull()?.onTouchInput(mask, x, y)
                }
                1 -> { // mouse
                    inputServiceOrNull()?.onMouseInput(mask, x, y)
                }
                else -> {
                }
            }
        }
    }

    @Keep
    @RequiresApi(Build.VERSION_CODES.N)
    fun rustKeyEventInput(input: ByteArray) {
        inputServiceOrNull()?.onKeyEvent(input)
    }

    @Keep
    fun rustGetByName(name: String): String {
        return when (name) {
            "screen_size" -> {
                JSONObject().apply {
                    put("width",SCREEN_INFO.width)
                    put("height",SCREEN_INFO.height)
                    put("scale",SCREEN_INFO.scale)
                }.toString()
            }
            "is_start" -> {
                isStart.toString()
            }
            else -> ""
        }
    }

    @Keep
    fun rustSetByName(name: String, arg1: String, arg2: String) {
        when (name) {
            "add_connection" -> {
                try {
                    val jsonObject = JSONObject(arg1)
                    val id = jsonObject["id"] as Int
                    val username = jsonObject["name"] as String
                    val peerId = jsonObject["peer_id"] as String
                    val authorized = jsonObject["authorized"] as Boolean
                    val isFileTransfer = jsonObject["is_file_transfer"] as Boolean
                    val type = if (isFileTransfer) {
                        translate("Transfer file")
                    } else {
                        translate("Share screen")
                    }
                    if (authorized) {
                        if (!isFileTransfer && !isStart) {
                            ensureCaptureStarted()
                        }
                        onClientAuthorizedNotification(id, type, username, peerId)
                    } else {
                        loginRequestNotification(id, type, username, peerId)
                    }
                } catch (e: JSONException) {
                    e.printStackTrace()
                }
            }
            "update_voice_call_state" -> {
                try {
                    val jsonObject = JSONObject(arg1)
                    val id = jsonObject["id"] as Int
                    val username = jsonObject["name"] as String
                    val peerId = jsonObject["peer_id"] as String
                    val inVoiceCall = jsonObject["in_voice_call"] as Boolean
                    val incomingVoiceCall = jsonObject["incoming_voice_call"] as Boolean
                    if (!inVoiceCall) {
                        if (incomingVoiceCall) {
                            voiceCallRequestNotification(id, "Voice Call Request", username, peerId)
                        } else {
                            if (!audioRecordHandle.switchOutVoiceCall(mediaProjection)) {
                                Log.e(logTag, "switchOutVoiceCall fail")
                                MainActivity.flutterMethodChannel?.invokeMethod("msgbox", mapOf(
                                    "type" to "custom-nook-nocancel-hasclose-error",
                                    "title" to "Voice call",
                                    "text" to "Failed to switch out voice call."))
                            }
                        }
                    } else {
                        if (!audioRecordHandle.switchToVoiceCall(mediaProjection)) {
                            Log.e(logTag, "switchToVoiceCall fail")
                            MainActivity.flutterMethodChannel?.invokeMethod("msgbox", mapOf(
                                "type" to "custom-nook-nocancel-hasclose-error",
                                "title" to "Voice call",
                                "text" to "Failed to switch to voice call."))
                        }
                    }
                } catch (e: JSONException) {
                    e.printStackTrace()
                }
            }
            "stop_capture" -> {
                Log.d(logTag, "from rust:stop_capture")
                stopCapture()
            }
            "half_scale" -> {
                val halfScale = arg1.toBoolean()
                if (isHalfScale != halfScale) {
                    isHalfScale = halfScale
                    updateScreenInfo(resources.configuration.orientation)
                }
                
            }
            else -> {
            }
        }
    }

    private var serviceLooper: Looper? = null
    private var serviceHandler: Handler? = null

    private val powerManager: PowerManager by lazy { applicationContext.getSystemService(Context.POWER_SERVICE) as PowerManager }
    private val wakeLock: PowerManager.WakeLock by lazy { powerManager.newWakeLock(PowerManager.ACQUIRE_CAUSES_WAKEUP or PowerManager.SCREEN_BRIGHT_WAKE_LOCK, "luoda:wakelock")}

    companion object {
        private var _isReady = false // media permission ready status
        private var _isStart = false // screen capture start status
        private var _isAudioStart = false // audio capture start status
        val isReady: Boolean
            get() = _isReady
        val isStart: Boolean
            get() = _isStart
        val isAudioStart: Boolean
            get() = _isAudioStart
    }

    private val logTag = "LOG_SERVICE"
    private val useVP9 = false
    private val binder = LocalBinder()

    private var reuseVirtualDisplay = Build.VERSION.SDK_INT >= 29

    // video
    private var mediaProjection: MediaProjection? = null
    private var surface: Surface? = null
    private val sendVP9Thread = Executors.newSingleThreadExecutor()
    private var videoEncoder: MediaCodec? = null
    private var imageReader: ImageReader? = null
    private var virtualDisplay: VirtualDisplay? = null

    /// Callback to detect MediaProjection revocation at runtime.
    /// Without this, _isReady stays true after the OS tears down the projection,
    /// preventing the app from re-requesting screen capture permission.
    ///
    /// 运行期竞态（2026-09-11 华为实测复现）：本回调跑在 `serviceHandler` 线程上，
    /// 而 EMUI/Android 会在「同机另一个 App 抢走投屏」时立刻撤销我们刚拿到的投影
    /// —— 撤销时刻可能正好落在 `startCapture()` 的执行中途。历史版本里
    /// `startCapture()` 在 null 检查之后又用 `mediaProjection!!` 二次解引用，
    /// 竞态命中就抛 NullPointerException 把整个进程带走。用户看到的现象是
    /// 「被控端 App 突然退出，主控端仍显示已连接但没有画面」。
    /// 现在：
    ///   1) `startCapture()` 全程只操作局部快照，绝不二次读字段；
    ///   2) 本回调把已建好的采集资源释放干净，避免 Surface/ImageReader 泄漏；
    ///   3) 只把状态标记为「投影已失效」，下一次连接自然会重新申请授权。
    private val projectionCallback = object : MediaProjection.Callback() {
        override fun onStop() {
            Log.w(logTag, "MediaProjection stopped by system; resetting state")
            val wasCapturing = _isStart
            _isReady = false
            _isStart = false
            mediaProjection = null
            // 投影已死 → VirtualDisplay 一并失效，必须真正释放而不是只暂停。
            releaseCaptureResources(forceReleaseDisplay = true)
            Handler(Looper.getMainLooper()).post {
                MainActivity.flutterMethodChannel?.invokeMethod(
                    "on_media_projection_canceled", null)
            }
            if (wasCapturing) {
                Log.w(
                    logTag,
                    "projection revoked mid-session; capture resources released, " +
                        "next connection will request a fresh grant"
                )
            }
        }
    }

    // audio
    private val audioRecordHandle = AudioRecordHandle(this, { isStart }, { isAudioStart })

    // notification
    private lateinit var notificationManager: NotificationManager
    private lateinit var notificationChannel: String
    private lateinit var notificationBuilder: NotificationCompat.Builder

    // 局域网发现保活锁，见 acquireLanDiscoveryLocks()
    private var lanMulticastLock: WifiManager.MulticastLock? = null
    private var lanWifiLock: WifiManager.WifiLock? = null

    /**
     * Rust 侧会绑定 `0.0.0.0:21119` 收 UDP 广播探测包，主控端（PC）据此在局域网内
     * 直接按 ID 找到本机。华为 / 荣耀等 ROM 的后台管控会把非单播报文直接从 WiFi
     * 驱动层丢掉，导致主控端只能退回服务器打洞；而手机端经 WebSocket 注册到 hbbs，
     * 服务端只拿到 X-Real-IP、端口被写死成 0，打洞在协议层必然失败 —— 表现就是
     * 「PC 能看到手机在线，但连不上」。
     *
     * 持有一个 MulticastLock 才能让驱动把非单播报文投递上来；WifiLock 则避免息屏后
     * 射频进入省电模式漏收广播。
     */
    private fun acquireLanDiscoveryLocks() {
        try {
            val wm = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
            if (wm == null) {
                Log.w(logTag, "lan discovery: WifiManager unavailable, skip locking")
                return
            }
            if (lanMulticastLock == null) {
                lanMulticastLock = wm.createMulticastLock("ldesk-lan-discovery").apply {
                    setReferenceCounted(false)
                }
            }
            lanMulticastLock?.let {
                if (!it.isHeld) {
                    it.acquire()
                    Log.d(logTag, "lan discovery: MulticastLock acquired")
                }
            }
            if (lanWifiLock == null) {
                lanWifiLock = wm.createWifiLock(
                    WifiManager.WIFI_MODE_FULL_HIGH_PERF, "ldesk-lan-discovery"
                ).apply {
                    setReferenceCounted(false)
                }
            }
            lanWifiLock?.let {
                if (!it.isHeld) {
                    it.acquire()
                    Log.d(logTag, "lan discovery: WifiLock acquired")
                }
            }
        } catch (e: Exception) {
            Log.e(logTag, "lan discovery: failed to acquire locks", e)
        }
    }

    private fun releaseLanDiscoveryLocks() {
        try {
            lanMulticastLock?.let { if (it.isHeld) it.release() }
            lanWifiLock?.let { if (it.isHeld) it.release() }
        } catch (e: Exception) {
            Log.e(logTag, "lan discovery: failed to release locks", e)
        }
    }

    override fun onCreate() {
        super.onCreate()
        Log.d(logTag,"MainService onCreate, sdk int:${Build.VERSION.SDK_INT} reuseVirtualDisplay:$reuseVirtualDisplay")
        FFI.init(this)
        HandlerThread("Service", Process.THREAD_PRIORITY_BACKGROUND).apply {
            start()
            serviceLooper = looper
            serviceHandler = Handler(looper)
        }
        updateScreenInfo(resources.configuration.orientation)
        initNotification()

        // keep the config dir same with flutter
        val prefs = applicationContext.getSharedPreferences(KEY_SHARED_PREFERENCES, FlutterActivity.MODE_PRIVATE)
        val configPath = prefs.getString(KEY_APP_DIR_CONFIG_PATH, "") ?: ""
        FFI.startServer(configPath, "")

        createForegroundNotification()

        // 局域网发现保活：Rust 侧绑定 21119 的广播监听是在 startServer 里异步起的，
        // 锁必须尽早持有，否则华为等 ROM 会丢掉探测包。
        acquireLanDiscoveryLocks()

        // 不要试图在这里恢复上次的投屏授权：MediaProjection 授权无法跨进程持久化
        // （结果 Intent 里没有可用的 data Uri，令牌是 IBinder 落不了盘）。
        // 详见 invalidateProjection() 下方的说明。进程重建后必须重新授权一次。
    }

    override fun onDestroy() {
        Log.d(logTag, "MainService onDestroy")
        // Clean up capture resources to prevent leaks when system kills the service
        try {
            stopCapture()
        } catch (e: Exception) {
            Log.e(logTag, "stopCapture in onDestroy failed", e)
        }
        // Unregister the projection callback if registered
        try {
            mediaProjection?.unregisterCallback(projectionCallback)
        } catch (e: Exception) {
            // ignore
        }
        checkMediaPermission()
        releaseLanDiscoveryLocks()
        super.onDestroy()
    }

    private var isHalfScale: Boolean? = null;
    private fun updateScreenInfo(orientation: Int) {
        var w: Int
        var h: Int
        var dpi: Int
        val windowManager = getSystemService(Context.WINDOW_SERVICE) as WindowManager

        @Suppress("DEPRECATION")
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val m = windowManager.maximumWindowMetrics
            w = m.bounds.width()
            h = m.bounds.height()
            dpi = resources.configuration.densityDpi
        } else {
            val dm = DisplayMetrics()
            windowManager.defaultDisplay.getRealMetrics(dm)
            w = dm.widthPixels
            h = dm.heightPixels
            dpi = dm.densityDpi
        }

        val max = max(w,h)
        val min = min(w,h)
        if (orientation == ORIENTATION_LANDSCAPE) {
            w = max
            h = min
        } else {
            w = min
            h = max
        }
        Log.d(logTag,"updateScreenInfo:w:$w,h:$h")
        var scale = 1
        if (w != 0 && h != 0) {
            if (isHalfScale == true && (w > MAX_SCREEN_SIZE || h > MAX_SCREEN_SIZE)) {
                scale = 2
                w /= scale
                h /= scale
                dpi /= scale
            }
            if (SCREEN_INFO.width != w) {
                SCREEN_INFO.width = w
                SCREEN_INFO.height = h
                SCREEN_INFO.scale = scale
                SCREEN_INFO.dpi = dpi
                if (isStart) {
                    stopCapture()
                    FFI.refreshScreen()
                    startCapture()
                } else {
                    FFI.refreshScreen()
                }
            }

        }
    }

    override fun onBind(intent: Intent): IBinder {
        Log.d(logTag, "service onBind")
        return binder
    }

    inner class LocalBinder : Binder() {
        init {
            Log.d(logTag, "LocalBinder init")
        }

        fun getService(): MainService = this@MainService
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        Log.d("whichService", "this service: ${Thread.currentThread()}")
        super.onStartCommand(intent, flags, startId)
        if (intent?.action == ACT_START_CAPTURE) {
            createForegroundNotification()
            ensureCaptureStarted()
            return START_NOT_STICKY
        }
        if (intent?.action == ACT_INIT_MEDIA_PROJECTION_AND_SERVICE) {
            createForegroundNotification()

            if (intent.getBooleanExtra(EXT_INIT_FROM_BOOT, false)) {
                FFI.startService()
            }
            Log.d(logTag, "service starting: ${startId}:${Thread.currentThread()}")
            val mediaProjectionManager =
                getSystemService(MEDIA_PROJECTION_SERVICE) as MediaProjectionManager

            intent.getParcelableExtra<Intent>(EXT_MEDIA_PROJECTION_RES_INTENT)?.let {
                mediaProjection =
                    mediaProjectionManager.getMediaProjection(Activity.RESULT_OK, it)
                // Register callback to detect runtime revocation
                mediaProjection?.registerCallback(projectionCallback, serviceHandler)
                checkMediaPermission()
                _isReady = true
                // 授权到手即在本次进程内生效（不落盘，见 invalidateProjection() 下方说明）。
                // If capture was started but VirtualDisplay creation failed (e.g. single-app mode),
                // retry with the new full MediaProjection
                if (_isStart && virtualDisplay == null) {
                    Log.d(logTag, "Retrying VirtualDisplay creation with new MediaProjection")
                    retryVirtualDisplay()
                }
                if (intent.getBooleanExtra(
                        EXT_START_CAPTURE_AFTER_PROJECTION, false) && !isStart) {
                    startCapture()
                }
            } ?: let {
                val fromBoot = intent.getBooleanExtra(EXT_INIT_FROM_BOOT, false)
                if (fromBoot) {
                    // 开机自启/后台拉起：只启动服务保持“可被远程连接/在线”，
                    // 绝不自动弹出系统录屏授权窗口。投屏授权只在用户主动进入
                    // “分享屏幕”并开启时由 Flutter 显式触发(带 res intent 重来)。
                    Log.d(logTag, "boot-start without media projection; skip auto permission dialog")
                } else {
                    // 无实际触发者(开机自启必带 boot=true, 主动投屏必带 res intent)。
                    // 为彻底满足"授权窗只在用户主动分享时弹出", 此兜底不再自动弹授权, 仅记录。
                    Log.d(logTag, "init intent without res intent and not from boot; no auto dialog");
                }
            }
        }
        return START_NOT_STICKY // don't use sticky (auto restart), the new service (from auto restart) will lose control
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        updateScreenInfo(newConfig.orientation)
    }

    private fun requestMediaProjection() {
        // Launch the transparent activity directly. Denial is now handled by
        // PermissionRequestTransparentActivity itself, which notifies Flutter
        // via on_media_projection_canceled regardless of who started it.
        val intent = Intent(this, PermissionRequestTransparentActivity::class.java).apply {
            action = ACT_REQUEST_MEDIA_PROJECTION
            flags = Intent.FLAG_ACTIVITY_NEW_TASK
        }
        startActivity(intent)
    }

    /// Start screen capture for an already-authorized session.
    ///
    /// 语义（2026-09-11 重新明确）：**授权一次，在本次进程存活期间一直复用**。
    /// 只要 `mediaProjection` 还在且 `isReady`，就直接起采集、绝不弹窗；
    /// 只有投影真的不存在（首次被控 / 进程被系统回收 / 投影被系统撤销）时，
    /// 才通过透明 Activity 走一次系统授权。
    /// 「进程重建后要重新授权」是 Android 平台约束（授权不可持久化，
    /// 见 invalidateProjection() 下方的说明），不是这里的逻辑问题。
    private fun ensureCaptureStarted() {
        if (isStart) return
        if (mediaProjection == null || !isReady) {
            Log.d(logTag, "ensureCaptureStarted: no live projection, requesting system grant once")
            val projectionIntent = Intent(this, PermissionRequestTransparentActivity::class.java).apply {
                action = ACT_REQUEST_MEDIA_PROJECTION
                flags = Intent.FLAG_ACTIVITY_NEW_TASK
                putExtra(EXT_START_CAPTURE_AFTER_PROJECTION, true)
            }
            startActivity(projectionIntent)
        } else {
            startCapture()
        }
    }

    @SuppressLint("WrongConstant")
    private fun createSurface(): Surface? {
        return if (useVP9) {
            // TODO
            null
        } else {
            Log.d(logTag, "ImageReader.newInstance:INFO:$SCREEN_INFO")
            imageReader =
                ImageReader.newInstance(
                    SCREEN_INFO.width,
                    SCREEN_INFO.height,
                    PixelFormat.RGBA_8888,
                    4
                ).apply {
                    setOnImageAvailableListener({ imageReader: ImageReader ->
                        try {
                            // If not call acquireLatestImage, listener will not be called again
                            imageReader.acquireLatestImage().use { image ->
                                if (image == null || !isStart) return@setOnImageAvailableListener
                                val planes = image.planes
                                val buffer = planes[0].buffer
                                buffer.rewind()
                                FFI.onVideoFrameUpdate(buffer)
                            }
                        } catch (ignored: java.lang.Exception) {
                        }
                    }, serviceHandler)
                }
            Log.d(logTag, "ImageReader.setOnImageAvailableListener done")
            imageReader?.surface
        }
    }

    fun onVoiceCallStarted(): Boolean {
        return audioRecordHandle.onVoiceCallStarted(mediaProjection)
    }

    fun onVoiceCallClosed(): Boolean {
        return audioRecordHandle.onVoiceCallClosed(mediaProjection)
    }

    /// 释放采集侧资源（VirtualDisplay / ImageReader / Surface / 编码器）。
    /// 可重复调用、可从任意线程调用：`onStop()` 在系统撤销投影时用它清理，
    /// `startCapture()` 失败回滚时也用它，避免「投影已经没了但 Surface 和
    /// ImageReader 还挂着」造成句柄泄漏与永久黑屏。
    ///
    /// [forceReleaseDisplay] 必须为 true 的情形：**底层 MediaProjection 已经死了**。
    /// 此时 VirtualDisplay 也一起失效，只能真正 release —— 否则会被
    /// `createOrSetVirtualDisplay()` 的 `virtualDisplay?.let { setSurface(...) }`
    /// 复用成「挂在死投影上的虚拟屏」，画面永远出不来。
    /// 而正常「暂停一次会话但投影仍有效」的场景（`stopCapture`）保持 false，
    /// 这样下一次连接能零成本复用同一个 VirtualDisplay。
    /// 注意：本方法与 `stopCapture()` 共用同一把对象锁（`@Synchronized`），
    /// 避免 serviceHandler 线程释放资源的同时主线程正在建 Surface。
    @Synchronized
    private fun releaseCaptureResources(forceReleaseDisplay: Boolean = false) {
        try {
            if (forceReleaseDisplay || !reuseVirtualDisplay) {
                virtualDisplay?.release()
                virtualDisplay = null
            } else {
                virtualDisplay?.setSurface(null)
            }
        } catch (e: Exception) {
            Log.w(logTag, "releaseCaptureResources: virtual display release failed: ${e.message}")
            virtualDisplay = null
        }
        // imageReader 必须在它持有的 surface 释放之前关闭
        try {
            imageReader?.close()
        } catch (e: Exception) {
            Log.w(logTag, "releaseCaptureResources: imageReader close failed: ${e.message}")
        }
        imageReader = null
        videoEncoder?.let {
            try {
                it.signalEndOfInputStream()
                it.stop()
                it.release()
            } catch (e: Exception) {
                Log.w(logTag, "releaseCaptureResources: encoder release failed: ${e.message}")
            }
        }
        videoEncoder = null
        try {
            surface?.release()
        } catch (e: Exception) {
            Log.w(logTag, "releaseCaptureResources: surface release failed: ${e.message}")
        }
        surface = null
    }

    fun startCapture(): Boolean {
        if (isStart && virtualDisplay != null) {
            return true  // Already capturing with a valid VirtualDisplay
        }
        // 只取一次局部快照。onStop() 跑在 serviceHandler 线程上，可能在下面任意
        // 一步把 `mediaProjection` 置空 —— 历史版本在这里读了两遍字段并用 `!!`
        // 解引用，于是「投影被系统撤销」升级成了进程崩溃。之后全程只用 mp。
        val mp = mediaProjection
        if (mp == null) {
            Log.w(logTag, "startCapture fail,mediaProjection is null")
            return false
        }

        if (!isStart) {
            updateScreenInfo(resources.configuration.orientation)
            Log.d(logTag, "Start Capture")
        } else {
            Log.d(logTag, "Retry Capture (VirtualDisplay was null)")
        }

        try {
            surface = createSurface()
            if (useVP9) {
                startVP9VideoRecorder(mp)
            } else {
                startRawVideoRecorder(mp)
            }
        } catch (e: RuntimeException) {
            // 快照与使用之间投影被系统撤销时（同机另一 App 抢投屏、用户在系统 UI
            // 里停止共享），createVirtualDisplay 会抛 IllegalStateException 或
            // NullPointerException。优雅失败并让下一次连接重新申请授权，
            // 绝不让进程崩溃。
            Log.e(
                logTag,
                "startCapture aborted, projection became invalid: " +
                    "${e.javaClass.simpleName}: ${e.message}"
            )
            FFI.setFrameRawEnable("video", false)
            // invalidateProjection() 内部会强制释放挂在死投影上的采集资源
            invalidateProjection()
            return false
        }

        // Only mark capture active after both the projection surface and the
        // VirtualDisplay exist.  A stale true here makes the Rust video service
        // wait forever for frames that will never arrive.
        val ok = surface != null && virtualDisplay != null
        if (!ok) {
            Log.e(logTag, "startCapture failed to create video surface/virtual display")
            FFI.setFrameRawEnable("video", false)
            releaseCaptureResources(forceReleaseDisplay = true)
            return false
        }
        _isStart = true
        FFI.setFrameRawEnable("video", true)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            if (!audioRecordHandle.createAudioRecorder(false, mp)) {
                Log.d(logTag, "createAudioRecorder fail")
            } else {
                Log.d(logTag, "audio recorder start")
                audioRecordHandle.startAudioRecorder()
            }
        }
        MainActivity.rdClipboardManager?.setCaptureStarted(_isStart)
        return true
    }

    /// Retry creating VirtualDisplay when mediaProjection was re-granted after a failed capture attempt.
    private fun retryVirtualDisplay() {
        if (mediaProjection == null || surface == null) {
            Log.w(logTag, "retryVirtualDisplay failed: mediaProjection or surface is null")
            return
        }
        Log.d(logTag, "retryVirtualDisplay: recreating VirtualDisplay")
        if (useVP9) {
            startVP9VideoRecorder(mediaProjection!!)
        } else {
            startRawVideoRecorder(mediaProjection!!)
        }
    }

    @Synchronized
    fun stopCapture() {
        Log.d(logTag, "Stop Capture")
        FFI.setFrameRawEnable("video",false)
        _isStart = false
        MainActivity.rdClipboardManager?.setCaptureStarted(_isStart)
        // release video
        if (reuseVirtualDisplay) {
            // The virtual display video projection can be paused by calling `setSurface(null)`.
            // https://developer.android.com/reference/android/hardware/display/VirtualDisplay.Callback
            // https://learn.microsoft.com/en-us/dotnet/api/android.hardware.display.virtualdisplay.callback.onpaused?view=net-android-34.0
            virtualDisplay?.setSurface(null)
        } else {
            virtualDisplay?.release()
        }
        // suface needs to be release after `imageReader.close()` to imageReader access released surface
        // https://github.com/luoda/luoda/issues/4118#issuecomment-1515666629
        imageReader?.close()
        imageReader = null
        videoEncoder?.let {
            it.signalEndOfInputStream()
            it.stop()
            it.release()
        }
        if (!reuseVirtualDisplay) {
            virtualDisplay = null
        }
        videoEncoder = null
        // suface needs to be release after `imageReader.close()` to imageReader access released surface
        // https://github.com/luoda/luoda/issues/4118#issuecomment-1515666629
        surface?.release()
        surface = null

        // release audio
        _isAudioStart = false
        audioRecordHandle.tryReleaseAudio()
    }

    fun destroy() {
        Log.d(logTag, "destroy service")
        _isReady = false
        _isAudioStart = false

        stopCapture()

        // Unregister projection callback before releasing
        try {
            mediaProjection?.unregisterCallback(projectionCallback)
        } catch (e: Exception) {
            // ignore
        }

        if (reuseVirtualDisplay) {
            virtualDisplay?.release()
            virtualDisplay = null
        }

        mediaProjection = null
        checkMediaPermission()
        stopForeground(true)
        stopSelf()
    }

    fun checkMediaPermission(): Boolean {
        Handler(Looper.getMainLooper()).post {
            MainActivity.flutterMethodChannel?.invokeMethod(
                "on_state_changed",
                mapOf("name" to "media", "value" to isReady.toString())
            )
        }
        Handler(Looper.getMainLooper()).post {
            MainActivity.flutterMethodChannel?.invokeMethod(
                "on_state_changed",
                mapOf("name" to "input", "value" to InputService.isOpen.toString())
            )
        }
        return isReady
    }

    private fun startRawVideoRecorder(mp: MediaProjection) {
        Log.d(logTag, "startRawVideoRecorder,screen info:$SCREEN_INFO")
        if (surface == null) {
            Log.d(logTag, "startRawVideoRecorder failed,surface is null")
            return
        }
        createOrSetVirtualDisplay(mp, surface!!)
    }

    private fun startVP9VideoRecorder(mp: MediaProjection) {
        createMediaCodec()
        videoEncoder?.let {
            surface = it.createInputSurface()
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                surface!!.setFrameRate(1F, FRAME_RATE_COMPATIBILITY_DEFAULT)
            }
            it.setCallback(cb)
            it.start()
            createOrSetVirtualDisplay(mp, surface!!)
        }
    }

    // https://github.com/bk138/droidVNC-NG/blob/b79af62db5a1c08ed94e6a91464859ffed6f4e97/app/src/main/java/net/christianbeier/droidvnc_ng/MediaProjectionService.java#L250
    // Reuse virtualDisplay if it exists, to avoid media projection confirmation dialog every connection.
    private fun createOrSetVirtualDisplay(mp: MediaProjection, s: Surface) {
        try {
            virtualDisplay?.let {
                it.resize(SCREEN_INFO.width, SCREEN_INFO.height, SCREEN_INFO.dpi)
                it.setSurface(s)
            } ?: let {
                virtualDisplay = mp.createVirtualDisplay(
                    "LDesk",
                    SCREEN_INFO.width, SCREEN_INFO.height, SCREEN_INFO.dpi, VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR,
                    s, null, null
                )
                if (virtualDisplay == null) {
                    // The projection token is stale (usually after the OS tore down a
                    // previous session). Re-requesting a confirmation dialog here is what
                    // makes the "share screen" prompt pop up after every session ends.
                    // Mark the projection invalid and let the next connection go through
                    // the normal init_service authorization flow instead of auto-prompting.
                    Log.e(logTag, "createVirtualDisplay returned null! Marking MediaProjection invalid.")
                    invalidateProjection()
                }
            }
        } catch (e: SecurityException) {
            Log.w(logTag, "createOrSetVirtualDisplay: got SecurityException, invalidating projection");
            invalidateProjection()
        } catch (e: IllegalStateException) {
            // 投影在 createVirtualDisplay 期间被系统停掉（同机另一 App 抢投屏、
            // 用户从系统 UI 停止共享）。按「投影已失效」处理，交给上层优雅失败，
            // 不要让异常冒到 ActivityThread 变成进程崩溃。
            Log.w(
                logTag,
                "createOrSetVirtualDisplay: projection no longer valid " +
                    "(${e.message}), invalidating projection"
            )
            invalidateProjection()
        } catch (e: NullPointerException) {
            // 少数 ROM 在投影已撤销时直接从 createVirtualDisplay 内部抛 NPE。
            Log.w(logTag, "createOrSetVirtualDisplay: NPE from a dead projection, invalidating")
            invalidateProjection()
        }
    }

    /// Clear the dead MediaProjection without prompting the user again.
    /// 之后 `ensureCaptureStarted()` 会看到 `mediaProjection == null`，
    /// 于是下一次被控连接走一次系统授权，而不是反复重试一个已死的投影。
    private fun invalidateProjection() {
        try {
            mediaProjection?.unregisterCallback(projectionCallback)
        } catch (_: Exception) {
        }
        mediaProjection = null
        _isReady = false
        _isStart = false
        // 投影已经死了：把挂在它上面的 VirtualDisplay / Surface 一并拆干净，
        // 否则下一次 startCapture 会复用一个失效的虚拟屏（永久黑屏）。
        releaseCaptureResources(forceReleaseDisplay = true)
        checkMediaPermission()
    }

    // ------------------------------------------------------------------
    // 关于「记住投屏授权」的说明（2026-09-11 结论）
    // ------------------------------------------------------------------
    //
    // 历史上这里实现过一套 saveProjectionToken / restoreProjectionToken：
    // 把授权结果 Intent 的 `data` Uri 存进 SharedPreferences，进程重建后
    // 用 `Intent().setData(uri)` + `getMediaProjection(RESULT_OK, ...)` 复原，
    // 以做到「授权一次，永久免弹窗」。
    //
    // 实测（华为 GNH0222922000982，Android 12 / EMUI，2.2.38）证明这条路走不通：
    //     W/LOG_SERVICE: saveProjectionToken: result intent has no data Uri,
    //                    grant cannot be persisted
    // 系统给回的结果 Intent 里根本没有 `data`，令牌是一个 IBinder（无法序列化落盘），
    // 因此这段代码从未成功保存过任何东西，`restoreProjectionToken()` 永远返回 false。
    // 保留它只会误导后来人以为存在跨进程复用，故整体删除。
    //
    // 正确的行为边界：
    //   * 进程存活期间 → 复用同一个 MediaProjection，被控连接**不弹窗**
    //     （见 ensureCaptureStarted / stopCapture：停采集不销毁投影）；
    //   * 进程被系统回收 / 投影被系统撤销 → 下一次连接**必须重新授权一次**。
    //     这是 Android 平台约束，不是缺陷。

    private val cb: MediaCodec.Callback = object : MediaCodec.Callback() {
        override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {}
        override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {}

        override fun onOutputBufferAvailable(
            codec: MediaCodec,
            index: Int,
            info: MediaCodec.BufferInfo
        ) {
            codec.getOutputBuffer(index)?.let { buf ->
                sendVP9Thread.execute {
                    val byteArray = ByteArray(buf.limit())
                    buf.get(byteArray)
                    // sendVp9(byteArray)
                    codec.releaseOutputBuffer(index, false)
                }
            }
        }

        override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
            Log.e(logTag, "MediaCodec.Callback error:$e")
        }
    }

    private fun createMediaCodec() {
        Log.d(logTag, "MediaFormat.MIMETYPE_VIDEO_VP9 :$MIME_TYPE")
        videoEncoder = MediaCodec.createEncoderByType(MIME_TYPE)
        val mFormat =
            MediaFormat.createVideoFormat(MIME_TYPE, SCREEN_INFO.width, SCREEN_INFO.height)
        mFormat.setInteger(MediaFormat.KEY_BIT_RATE, VIDEO_KEY_BIT_RATE)
        mFormat.setInteger(MediaFormat.KEY_FRAME_RATE, VIDEO_KEY_FRAME_RATE)
        mFormat.setInteger(
            MediaFormat.KEY_COLOR_FORMAT,
            MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Flexible
        )
        mFormat.setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 5)
        try {
            videoEncoder!!.configure(mFormat, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        } catch (e: Exception) {
            Log.e(logTag, "mEncoder.configure fail!")
        }
    }

    private fun initNotification() {
        notificationManager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        notificationChannel = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channelId = "LDesk"
            val channelName = "LDesk Service"
            val channel = NotificationChannel(
                channelId,
                channelName, NotificationManager.IMPORTANCE_HIGH
            ).apply {
                description = "LDesk Service Channel"
            }
            channel.lightColor = Color.BLUE
            channel.lockscreenVisibility = Notification.VISIBILITY_PRIVATE
            notificationManager.createNotificationChannel(channel)
            channelId
        } else {
            ""
        }
        notificationBuilder = NotificationCompat.Builder(this, notificationChannel)
    }

    @SuppressLint("UnspecifiedImmutableFlag")
    private fun createForegroundNotification() {
        val intent = Intent(this, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED
            action = Intent.ACTION_MAIN
            addCategory(Intent.CATEGORY_LAUNCHER)
            putExtra("type", type)
        }
        val pendingIntent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            PendingIntent.getActivity(this, 0, intent, FLAG_UPDATE_CURRENT or FLAG_IMMUTABLE)
        } else {
            PendingIntent.getActivity(this, 0, intent, FLAG_UPDATE_CURRENT)
        }
        val notification = notificationBuilder
            .setOngoing(true)
            .setSmallIcon(R.mipmap.ic_stat_logo)
            .setDefaults(Notification.DEFAULT_ALL)
            .setAutoCancel(true)
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .setContentTitle(DEFAULT_NOTIFY_TITLE)
            .setContentText(translate(DEFAULT_NOTIFY_TEXT))
            .setOnlyAlertOnce(true)
            .setContentIntent(pendingIntent)
            .setColor(ContextCompat.getColor(this, R.color.primary))
            .setWhen(System.currentTimeMillis())
            .build()
        startForeground(DEFAULT_NOTIFY_ID, notification)
    }

    private fun loginRequestNotification(
        clientID: Int,
        type: String,
        username: String,
        peerId: String
    ) {
        val notification = notificationBuilder
            .setOngoing(false)
            .setPriority(NotificationCompat.PRIORITY_MAX)
            .setContentTitle(translate("Do you want to accept remote assistance?"))
            .setContentText("$type:$username-$peerId")
            // .setStyle(MediaStyle().setShowActionsInCompactView(0, 1))
            // .addAction(R.drawable.check_blue, "check", genLoginRequestPendingIntent(true))
            // .addAction(R.drawable.close_red, "close", genLoginRequestPendingIntent(false))
            .build()
        notificationManager.notify(getClientNotifyID(clientID), notification)
    }

    private fun onClientAuthorizedNotification(
        clientID: Int,
        type: String,
        username: String,
        peerId: String
    ) {
        cancelNotification(clientID)
        val notification = notificationBuilder
            .setOngoing(false)
            .setPriority(NotificationCompat.PRIORITY_MAX)
            .setContentTitle("$type ${translate("Established")}")
            .setContentText("$username - $peerId")
            .build()
        notificationManager.notify(getClientNotifyID(clientID), notification)
    }

    private fun voiceCallRequestNotification(
        clientID: Int,
        type: String,
        username: String,
        peerId: String
    ) {
        val notification = notificationBuilder
            .setOngoing(false)
            .setPriority(NotificationCompat.PRIORITY_MAX)
            .setContentTitle(translate("Do you accept?"))
            .setContentText("$type:$username-$peerId")
            .build()
        notificationManager.notify(getClientNotifyID(clientID), notification)
    }

    private fun getClientNotifyID(clientID: Int): Int {
        return clientID + NOTIFY_ID_OFFSET
    }

    fun cancelNotification(clientID: Int) {
        notificationManager.cancel(getClientNotifyID(clientID))
    }

    @SuppressLint("UnspecifiedImmutableFlag")
    private fun genLoginRequestPendingIntent(res: Boolean): PendingIntent {
        val intent = Intent(this, MainService::class.java).apply {
            action = ACT_LOGIN_REQ_NOTIFY
            putExtra(EXT_LOGIN_REQ_NOTIFY, res)
        }
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
            PendingIntent.getService(this, 111, intent, FLAG_IMMUTABLE)
        } else {
            PendingIntent.getService(this, 111, intent, FLAG_UPDATE_CURRENT)
        }
    }

    private fun setTextNotification(_title: String?, _text: String?) {
        val title = _title ?: DEFAULT_NOTIFY_TITLE
        val text = _text ?: translate(DEFAULT_NOTIFY_TEXT)
        val notification = notificationBuilder
            .clearActions()
            .setStyle(null)
            .setContentTitle(title)
            .setContentText(text)
            .build()
        notificationManager.notify(DEFAULT_NOTIFY_ID, notification)
    }
}
