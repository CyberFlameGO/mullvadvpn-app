package net.mullvad.mullvadvpn.viewmodel

import androidx.annotation.StringRes
import androidx.lifecycle.viewModelScope
import io.mockk.Runs
import io.mockk.coEvery
import io.mockk.every
import io.mockk.just
import io.mockk.mockk
import io.mockk.unmockkAll
import io.mockk.verify
import io.mockk.verifyAll
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import net.mullvad.mullvadvpn.R
import net.mullvad.mullvadvpn.TestCoroutineRule
import net.mullvad.mullvadvpn.applist.AppData
import net.mullvad.mullvadvpn.applist.ApplicationsProvider
import net.mullvad.mullvadvpn.applist.ViewIntent
import net.mullvad.mullvadvpn.assertLists
import net.mullvad.mullvadvpn.model.ListItemData
import net.mullvad.mullvadvpn.model.WidgetState
import net.mullvad.mullvadvpn.service.SplitTunneling
import org.junit.After
import org.junit.Before
import org.junit.Rule
import org.junit.Test

class SplitTunnelingViewModelTest {
    @get:Rule
    val testCoroutineRule = TestCoroutineRule()
    private val mockedApplicationsProvider = mockk<ApplicationsProvider>()
    private val mockedSplitTunneling = mockk<SplitTunneling>()
    private val appsProviderDeferred = CompletableDeferred<List<AppData>>()
    private lateinit var testSubject: SplitTunnelingViewModel

    @Before
    fun setup() {
        every { mockedSplitTunneling.enabled } returns true
        coEvery { mockedApplicationsProvider.getAppsListAsync() } returns appsProviderDeferred
        testSubject = SplitTunnelingViewModel(
            mockedApplicationsProvider,
            mockedSplitTunneling
        )
        Thread.sleep(50)
    }

    @After
    fun tearDown() {
        testSubject.viewModelScope.coroutineContext.cancel()
        unmockkAll()
    }

    @Test
    fun test_has_progress_on_start() = runBlocking {
        val actualList: List<ListItemData> = testSubject.listItems.first()
        val initialExpectedList = listOf(
            createTextItem(R.string.split_tunneling_description),
            createDivider(0), createProgressItem()
        )

        assertLists(initialExpectedList, actualList)

        verify(exactly = 1) {
            mockedApplicationsProvider.getAppsListAsync()
        }
    }

    @Test
    fun test_empty_app_list() = runBlocking {
        val flow = testSubject.listItems
        async {
            testSubject.processIntent(ViewIntent.ViewIsReady)
            appsProviderDeferred.complete(emptyList())
        }
        val actualList = flow.drop(1).first()
        val expectedList = listOf(createTextItem(R.string.split_tunneling_description))
        assertLists(expectedList, actualList)
    }

    @Test
    fun test_apps_list_delivered() = runBlocking {
        val appExcluded = AppData("test.excluded", 0, "testName1")
        val appNotExcluded = AppData("test.not.excluded", 0, "testName2")
        every { mockedSplitTunneling.excludedAppList } returns listOf(appExcluded.packageName)

        testSubject.processIntent(ViewIntent.ViewIsReady)
        appsProviderDeferred.complete(listOf(appExcluded, appNotExcluded))

        val actualList = testSubject.listItems.drop(1).first()
        val expectedList = listOf(
            createTextItem(R.string.split_tunneling_description),
            createDivider(0),
            createMainItem(R.string.exclude_applications),
            createApplicationItem(appExcluded, true),
            createDivider(1),
            createMainItem(R.string.all_applications),
            createApplicationItem(appNotExcluded, false),
        )

        assertLists(expectedList, actualList)
        verifyAll {
            mockedSplitTunneling.enabled
            mockedSplitTunneling.excludedAppList
            mockedSplitTunneling.excludedAppList
        }
    }

    @Test
    fun test_remove_app_from_excluded() = runBlocking {
        val flow = testSubject.listItems.drop(1)
        val app = AppData("test", 0, "testName")
        every { mockedSplitTunneling.excludedAppList } returns listOf(app.packageName)
        every { mockedSplitTunneling.includeApp(app.packageName) } just Runs
        async {
            testSubject.processIntent(ViewIntent.ViewIsReady)
            appsProviderDeferred.complete(listOf(app))
        }

        val listBeforeAction = flow.first()
        val expectedListBeforeAction = listOf(
            createTextItem(R.string.split_tunneling_description),
            createDivider(0),
            createMainItem(R.string.exclude_applications),
            createApplicationItem(app, true),
        )

        assertLists(expectedListBeforeAction, listBeforeAction)

        val item = listBeforeAction.first { it.identifier == app.packageName }
        testSubject.processIntent(ViewIntent.ChangeApplicationGroup(item))

        val itemsAfterAction = flow.first()
        val expectedList = listOf(
            createTextItem(R.string.split_tunneling_description),
            createDivider(1),
            createMainItem(R.string.all_applications),
            createApplicationItem(app, false),
        )

        assertLists(expectedList, itemsAfterAction)

        verifyAll {
            mockedSplitTunneling.enabled
            mockedSplitTunneling.excludedAppList
            mockedSplitTunneling.includeApp(app.packageName)
        }
    }

    @Test
    fun test_add_app_to_excluded() = runBlocking {
        val flow = testSubject.listItems.drop(1)
        val app = AppData("test", 0, "testName")
        every { mockedSplitTunneling.excludedAppList } returns emptyList()
        every { mockedSplitTunneling.excludeApp(app.packageName) } just Runs
        async {
            testSubject.processIntent(ViewIntent.ViewIsReady)
            appsProviderDeferred.complete(listOf(app))
        }

        val listBeforeAction = flow.first()
        val expectedListBeforeAction = listOf(
            createTextItem(R.string.split_tunneling_description),
            createDivider(1),
            createMainItem(R.string.all_applications),
            createApplicationItem(app, false),
        )

        assertLists(expectedListBeforeAction, listBeforeAction)

        val item = listBeforeAction.first { it.identifier == app.packageName }
        testSubject.processIntent(ViewIntent.ChangeApplicationGroup(item))

        val itemsAfterAction = flow.first()
        val expectedList = listOf(
            createTextItem(R.string.split_tunneling_description),
            createDivider(0),
            createMainItem(R.string.exclude_applications),
            createApplicationItem(app, true),
        )

        assertLists(expectedList, itemsAfterAction)

        verifyAll {
            mockedSplitTunneling.enabled
            mockedSplitTunneling.excludedAppList
            mockedSplitTunneling.excludeApp(app.packageName)
        }
    }

    private fun createApplicationItem(
        appData: AppData,
        checked: Boolean
    ): ListItemData = ListItemData.build(appData.packageName) {
        type = ListItemData.APPLICATION
        text = appData.name
        iconRes = appData.iconRes
        action = ListItemData.ItemAction(appData.packageName)
        widget = WidgetState.ImageState(
            if (checked) R.drawable.ic_icons_remove else R.drawable.ic_icons_add
        )
    }

    private fun createDivider(id: Int): ListItemData = ListItemData.build("space_$id") {
        type = ListItemData.DIVIDER
    }

    private fun createMainItem(@StringRes text: Int): ListItemData =
        ListItemData.build("header_$text") {
            type = ListItemData.ACTION
            textRes = text
        }

    private fun createTextItem(@StringRes text: Int): ListItemData =
        ListItemData.build("text_$text") {
            type = ListItemData.PLAIN
            textRes = text
            action = ListItemData.ItemAction(text.toString())
        }

    private fun createProgressItem(): ListItemData = ListItemData.build(identifier = "progress") {
        type = ListItemData.PROGRESS
    }
}
