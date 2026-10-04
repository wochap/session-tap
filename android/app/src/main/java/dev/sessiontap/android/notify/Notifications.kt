package dev.sessiontap.android.notify

import android.Manifest
import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.core.net.toUri
import dev.sessiontap.android.MainActivity
import dev.sessiontap.android.R
import dev.sessiontap.android.data.AgentKey
import dev.sessiontap.android.data.AgentNotifier
import dev.sessiontap.android.data.HubEntity
import dev.sessiontap.android.domain.AgentNotice
import dev.sessiontap.android.domain.NotifyChannel

object Channels {
    const val ATTENTION = "attention"
    const val COMPLETED = "completed"
    const val SERVICE = "service"

    fun create(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannels(
            listOf(
                NotificationChannel(ATTENTION, "Needs attention", NotificationManager.IMPORTANCE_HIGH).apply {
                    description = "An agent needs your permission or input"
                    lockscreenVisibility = Notification.VISIBILITY_PRIVATE
                },
                NotificationChannel(COMPLETED, "Finished", NotificationManager.IMPORTANCE_DEFAULT).apply {
                    description = "An agent finished its response"
                    lockscreenVisibility = Notification.VISIBILITY_PRIVATE
                },
                NotificationChannel(SERVICE, "Connection", NotificationManager.IMPORTANCE_MIN).apply {
                    description = "Ongoing connection to your hubs"
                    setShowBadge(false)
                },
            ),
        )
    }
}

/** POST_NOTIFICATIONS is a runtime permission from API 33; before that only the app toggle matters. */
fun notificationsAllowed(context: Context): Boolean {
    val enabled = NotificationManagerCompat.from(context).areNotificationsEnabled()
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return enabled
    return enabled && ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
}

/** Deep link that opens one session's detail screen. */
fun sessionUri(key: AgentKey) =
    "sessiontap://session/${key.hubId}/${key.sourceId}/${key.invocationId}".toUri()

class NotificationPoster(private val context: Context) : AgentNotifier {
    private val manager = NotificationManagerCompat.from(context)

    private fun allowed(): Boolean = notificationsAllowed(context)

    @SuppressLint("MissingPermission") // checked by allowed()
    override fun post(hub: HubEntity, key: AgentKey, notice: AgentNotice) {
        if (!allowed()) return
        val channel = if (notice.channel == NotifyChannel.Attention) Channels.ATTENTION else Channels.COMPLETED
        val open = PendingIntent.getActivity(
            context,
            key.notificationId,
            Intent(Intent.ACTION_VIEW, sessionUri(key), context, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val mute = PendingIntent.getBroadcast(
            context,
            hub.hubId.hashCode(),
            Intent(context, MuteReceiver::class.java).setAction(MuteReceiver.ACTION).putExtra(MuteReceiver.EXTRA_HUB, hub.hubId),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val expanded = listOf(notice.text, notice.body, notice.footer).filter { it.isNotEmpty() }.joinToString("\n")
        val public = NotificationCompat.Builder(context, channel)
            .setSmallIcon(R.drawable.ic_stat_broadcast)
            .setContentTitle(notice.publicTitle)
            .setContentText("Unlock to see details")
            .setSubText(hub.name)
            .build()
        val notification = NotificationCompat.Builder(context, channel)
            .setSmallIcon(R.drawable.ic_stat_broadcast)
            .setContentTitle(notice.title)
            .setContentText(notice.text)
            .setSubText(hub.name)
            .setStyle(NotificationCompat.BigTextStyle().bigText(expanded))
            .setCategory(if (notice.channel == NotifyChannel.Attention) NotificationCompat.CATEGORY_MESSAGE else NotificationCompat.CATEGORY_STATUS)
            .setPriority(if (notice.channel == NotifyChannel.Attention) NotificationCompat.PRIORITY_HIGH else NotificationCompat.PRIORITY_DEFAULT)
            .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
            .setPublicVersion(public)
            .setGroup(hub.hubId)
            .setAutoCancel(true)
            .setContentIntent(open)
            .addAction(0, "Open", open)
            .addAction(0, "Mute ${hub.name} 1h", mute)
            .build()
        manager.notify(key.notificationId, notification)
        postSummary(hub)
    }

    @SuppressLint("MissingPermission") // checked by allowed()
    private fun postSummary(hub: HubEntity) {
        if (!allowed()) return
        val summary = NotificationCompat.Builder(context, Channels.ATTENTION)
            .setSmallIcon(R.drawable.ic_stat_broadcast)
            .setContentTitle(hub.name)
            .setSubText(hub.name)
            .setGroup(hub.hubId)
            .setGroupSummary(true)
            .setGroupAlertBehavior(NotificationCompat.GROUP_ALERT_CHILDREN)
            .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
            .setAutoCancel(true)
            .build()
        manager.notify(summaryId(hub.hubId), summary)
    }

    override fun cancel(key: AgentKey) {
        manager.cancel(key.notificationId)
        val remaining = manager.activeNotifications.count { it.notification.group == key.hubId && it.id != summaryId(key.hubId) && it.id != key.notificationId }
        if (remaining == 0) manager.cancel(summaryId(key.hubId))
    }

    override fun cancelHub(hubId: String) {
        manager.activeNotifications.filter { it.notification.group == hubId }.forEach { manager.cancel(it.id) }
        manager.cancel(summaryId(hubId))
    }

    companion object {
        fun summaryId(hubId: String) = "summary|$hubId".hashCode()
        const val SERVICE_ID = 1
    }
}
