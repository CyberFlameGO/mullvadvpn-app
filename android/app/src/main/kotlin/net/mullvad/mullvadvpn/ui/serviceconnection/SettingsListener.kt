package net.mullvad.mullvadvpn.ui.serviceconnection

import android.os.Messenger
import net.mullvad.mullvadvpn.ipc.Event
import net.mullvad.mullvadvpn.ipc.EventDispatcher
import net.mullvad.mullvadvpn.ipc.Request
import net.mullvad.mullvadvpn.model.DnsOptions
import net.mullvad.mullvadvpn.model.RelaySettings
import net.mullvad.mullvadvpn.model.Settings
import net.mullvad.talpid.util.EventNotifier

class SettingsListener(private val connection: Messenger, eventDispatcher: EventDispatcher) {
    val accountNumberNotifier = EventNotifier<String?>(null)
    val dnsOptionsNotifier = EventNotifier<DnsOptions?>(null)
    val relaySettingsNotifier = EventNotifier<RelaySettings?>(null)
    val settingsNotifier = EventNotifier<Settings?>(null)

    private var settings by settingsNotifier.notifiable()

    var account: String?
        get() = accountNumberNotifier.latestEvent
        set(value) {
            connection.send(Request.SetAccount(value).message)
        }

    var allowLan: Boolean
        get() = settingsNotifier.latestEvent?.allowLan ?: false
        set(value) {
            connection.send(Request.SetAllowLan(value).message)
        }

    var autoConnect: Boolean
        get() = settingsNotifier.latestEvent?.autoConnect ?: false
        set(value) {
            connection.send(Request.SetAutoConnect(value).message)
        }

    var wireguardMtu: Int?
        get() = settingsNotifier.latestEvent?.tunnelOptions?.wireguard?.options?.mtu
        set(value) {
            connection.send(Request.SetWireGuardMtu(value).message)
        }

    init {
        eventDispatcher.registerHandler(Event.SettingsUpdate::class, ::handleNewEvent)
    }

    fun onDestroy() {
        accountNumberNotifier.unsubscribeAll()
        dnsOptionsNotifier.unsubscribeAll()
        relaySettingsNotifier.unsubscribeAll()
        settingsNotifier.unsubscribeAll()
    }

    private fun handleNewEvent(event: Event.SettingsUpdate) {
        event.settings?.let { settings -> handleNewSettings(settings) }
    }

    private fun handleNewSettings(newSettings: Settings) {
        // TODO: Skip until device integration is ready.
        // if (settings?.accountToken != newSettings.accountToken) {
        //     accountNumberNotifier.notify(newSettings.accountToken)
        // }

        if (settings?.tunnelOptions?.dnsOptions != newSettings.tunnelOptions.dnsOptions) {
            dnsOptionsNotifier.notify(newSettings.tunnelOptions.dnsOptions)
        }

        if (settings?.relaySettings != newSettings.relaySettings) {
            relaySettingsNotifier.notify(newSettings.relaySettings)
        }

        settings = newSettings
    }
}
