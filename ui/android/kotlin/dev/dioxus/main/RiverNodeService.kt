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

class RiverNodeService : Service() {

    companion object {
        const val CHANNEL_ID = "river_node_channel"
        const val NOTIFICATION_ID = 1
        const val ACTION_STOP = "dev.dioxus.main.action.STOP_NODE"
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
