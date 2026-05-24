/*
 * River MainActivity — extends the dx-generated WryActivity to start
 * RiverNodeService as a foreground service immediately on launch.
 *
 * Without this, Android's OOM killer can — and on Pixel-class devices
 * typically does — reap the process within seconds of the user
 * backgrounding the app, dropping every peer connection the embedded
 * Freenet node has built up. With the service running, the process is
 * categorised FOREGROUND_SERVICE for as long as the user keeps River
 * installed and the user hasn't tapped the "Stop" action.
 *
 * This file is part of the `ui/android/` overlay copied into
 * `target/dx/river-ui/.../kotlin/dev/dioxus/main/` by
 * `scripts/apply-android-overlay.sh`. It overwrites the empty
 * MainActivity stub dx generates. If dx regenerates the stub on a
 * fresh build, the overlay script re-applies on top.
 */
package dev.dioxus.main

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.core.app.ActivityCompat

// re-export buildconfig down from the parent (matches the dx stub)
import org.freenet.river.BuildConfig
typealias BuildConfig = BuildConfig

class MainActivity : WryActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // POST_NOTIFICATIONS is a runtime permission on API 33+ — without
        // it the foreground-service notification is hidden, defeating
        // the "ongoing presence" UX. We ask once on first launch; if
        // denied, the FGS still runs but is invisible to the user (which
        // is fine — Android keeps the process alive either way).
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            val granted = ActivityCompat.checkSelfPermission(
                this,
                Manifest.permission.POST_NOTIFICATIONS,
            ) == PackageManager.PERMISSION_GRANTED
            if (!granted) {
                ActivityCompat.requestPermissions(
                    this,
                    arrayOf(Manifest.permission.POST_NOTIFICATIONS),
                    REQUEST_CODE_POST_NOTIFICATIONS,
                )
            }
        }

        RiverNodeService.start(this)
    }

    companion object {
        private const val REQUEST_CODE_POST_NOTIFICATIONS = 1001
    }
}
