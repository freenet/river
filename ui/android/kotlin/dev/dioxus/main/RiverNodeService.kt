/*
 * Foreground service that keeps the embedded Freenet node alive when
 * the user backgrounds the app. Without this, Android can — and on
 * Pixel-class devices typically will — kill the process within a few
 * seconds of the user pressing Home, severing the peer connections
 * the node had managed to bring up.
 *
 * The service body is intentionally minimal: it posts the ongoing
 * notification (Android requires foreground services to do so) and
 * handles the "Stop" intent the notification action sends. The node
 * itself runs on a tokio runtime spawned by Rust in
 * `ui/src/node_runtime.rs::start_embedded_node()` — the service just
 * keeps the process category at FOREGROUND_SERVICE so the OOM killer
 * leaves us alone.
 *
 * Shutdown signaling: when the user taps "Stop" in the notification,
 * the resulting `ACTION_STOP` intent calls `stopSelf()` which fires
 * `onDestroy()`. Inside `onDestroy()` we invoke `nativeOnServiceStop`,
 * the JNI symbol exported from `node_runtime.rs::android` (see the
 * `tokio::sync::oneshot::Sender` parked in `SHUTDOWN_TX` there). Rust
 * sends the oneshot signal, which unblocks `run_node()` and lets it
 * drop the tokio runtime cleanly before the process exits.
 *
 * If `nativeOnServiceStop` is missing at runtime (e.g. the user
 * backgrounded the app before `start_embedded_node()` ever loaded
 * libdioxusmain), the JNI call throws `UnsatisfiedLinkError`; we
 * catch and ignore so the service can still tear itself down.
 */
package dev.dioxus.main

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat

class RiverNodeService : Service() {

    companion object {
        const val CHANNEL_ID = "river_node_channel"
        const val NOTIFICATION_ID = 1
        const val ACTION_STOP = "dev.dioxus.main.action.STOP_NODE"
        // Separate channel for incoming chat messages — IMPORTANCE_DEFAULT
        // so heads-up + sound + vibration fire while the user is on another
        // app or the lock screen. The foreground-service channel above stays
        // IMPORTANCE_LOW so the "Freenet node running" notification doesn't
        // interrupt the user every relaunch.
        const val MESSAGE_CHANNEL_ID = "river_messages"
        // Constant id so per-room tags partition the notification slot;
        // chosen above NOTIFICATION_ID (1) to keep them separate.
        const val MESSAGE_NOTIFICATION_ID = 100
        private const val TAG = "RiverNodeService"

        fun start(context: Context) {
            val intent = Intent(context, RiverNodeService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }

        // Registered on the application context. Safe to call repeatedly —
        // NotificationManager.createNotificationChannel is idempotent.
        fun registerChannel(context: Context) {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
            val nm = context.getSystemService(NotificationManager::class.java) ?: return
            val channel = NotificationChannel(
                CHANNEL_ID,
                "Freenet Node",
                NotificationManager.IMPORTANCE_LOW,
            ).apply {
                description = "Keeps the embedded Freenet node alive while River runs in the background"
                setShowBadge(false)
            }
            nm.createNotificationChannel(channel)
        }

        /**
         * Register the new-message channel. Idempotent on every Android
         * version. Called lazily from [postMessageNotification] so the
         * channel is only created on the first arriving message — keeps
         * the channel list tidy when notifications never fire on a fresh
         * install.
         */
        fun registerMessageChannel(context: Context) {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
            val nm = context.getSystemService(NotificationManager::class.java) ?: return
            if (nm.getNotificationChannel(MESSAGE_CHANNEL_ID) != null) return
            val channel = NotificationChannel(
                MESSAGE_CHANNEL_ID,
                "New messages",
                NotificationManager.IMPORTANCE_DEFAULT,
            ).apply {
                description = "Heads-up alerts when someone sends a message to a room you're in"
                enableLights(true)
                enableVibration(true)
                setShowBadge(true)
            }
            nm.createNotificationChannel(channel)
        }

        /**
         * Post (or update) a new-message notification.
         *
         * Called from Rust via JNI when the embedded node's room
         * synchronizer receives a message the user should be notified
         * about (see `ui/src/components/app/notifications.rs::show_notification`).
         *
         * The `tag` is a stable per-room identifier so subsequent
         * messages in the same room replace the previous notification
         * (rather than stacking N unread notifications per room). On
         * tap the launcher intent re-opens MainActivity, restoring the
         * existing process if it's still alive (FGS keeps it warm).
         *
         * SecurityException is caught because POST_NOTIFICATIONS is a
         * runtime permission on API 33+ — if the user declined it on
         * first launch, NotificationManagerCompat.notify throws. Best
         * effort: log and continue rather than crashing the worker
         * thread.
         */
        @JvmStatic
        fun postMessageNotification(context: Context, title: String, body: String, tag: String) {
            val app = context.applicationContext ?: context
            registerMessageChannel(app)

            val launchIntent = app.packageManager.getLaunchIntentForPackage(app.packageName)
                ?.apply { addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP) }
            val launchPi = launchIntent?.let {
                PendingIntent.getActivity(
                    app,
                    0,
                    it,
                    PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
                )
            }

            val builder = NotificationCompat.Builder(app, MESSAGE_CHANNEL_ID)
                .setContentTitle(title)
                .setContentText(body)
                .setStyle(NotificationCompat.BigTextStyle().bigText(body))
                .setSmallIcon(android.R.drawable.stat_notify_chat)
                .setPriority(NotificationCompat.PRIORITY_DEFAULT)
                .setCategory(NotificationCompat.CATEGORY_MESSAGE)
                .setAutoCancel(true)
                .setDefaults(NotificationCompat.DEFAULT_ALL)
            launchPi?.let { builder.setContentIntent(it) }

            try {
                NotificationManagerCompat.from(app).notify(tag, MESSAGE_NOTIFICATION_ID, builder.build())
            } catch (e: SecurityException) {
                Log.w(TAG, "postMessageNotification suppressed (POST_NOTIFICATIONS not granted): $e")
            } catch (t: Throwable) {
                Log.w(TAG, "postMessageNotification failed: $t")
            }
        }
    }

    override fun onCreate() {
        super.onCreate()
        registerChannel(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            Log.i(TAG, "ACTION_STOP received — stopping service")
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            return START_NOT_STICKY
        }
        startForeground(NOTIFICATION_ID, buildNotification())
        return START_STICKY
    }

    private fun buildNotification(): Notification {
        val stopIntent = Intent(this, RiverNodeService::class.java).apply {
            action = ACTION_STOP
        }
        val stopPi = PendingIntent.getService(
            this,
            0,
            stopIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )

        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("River")
            .setContentText("Freenet node running")
            .setSmallIcon(android.R.drawable.stat_notify_sync)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .addAction(android.R.drawable.ic_menu_close_clear_cancel, "Stop", stopPi)
            .build()
    }

    override fun onDestroy() {
        try {
            nativeOnServiceStop()
        } catch (t: UnsatisfiedLinkError) {
            // Native lib not loaded yet — nothing for Rust to tear down.
        } catch (t: Throwable) {
            Log.w(TAG, "nativeOnServiceStop threw $t")
        }
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private external fun nativeOnServiceStop()
}
