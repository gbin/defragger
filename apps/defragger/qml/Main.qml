import QtQuick
import QtQuick.Window
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import net.gootz.defragger

Kirigami.ApplicationWindow {
    id: window
    width: 1000
    height: 720
    minimumWidth: 760
    minimumHeight: 560
    visible: true
    title: qsTr("Defragger")

    property int selectedIndex: -1
    readonly property bool hasSelectedVolume: selectedIndex >= 0
        && selectedIndex < controller.volume_count
    readonly property string selectedVolumeId: hasSelectedVolume
        ? String(controller.volume_id(selectedIndex)) : ""
    readonly property bool selectedHasReport: hasSelectedVolume
        && controller.has_report
        && selectedVolumeId === String(controller.report_volume_id)
    readonly property bool selectedRequiresUnmount: hasSelectedVolume
        && controller.volume_revision >= 0
        && controller.volume_requires_unmount(selectedIndex)
    readonly property bool selectedIsBeingAnalyzed: hasSelectedVolume
        && controller.busy
        && controller.active_operation === "analysis"
        && selectedVolumeId === String(controller.analyzing_volume_id)
    readonly property bool selectedIsBeingOptimized: hasSelectedVolume
        && controller.busy
        && (controller.active_operation === "defragmentation"
            || controller.active_operation === "compaction")
        && selectedVolumeId === String(controller.analyzing_volume_id)
    readonly property bool selectedIsActive: selectedIsBeingAnalyzed
        || selectedIsBeingOptimized
    readonly property real optimizationProgress: {
        const files = controller.defrag_files_total > 0
            ? controller.defrag_files_completed / controller.defrag_files_total : 0
        const bytes = controller.defrag_bytes_total > 0
            ? controller.defrag_bytes_moved / controller.defrag_bytes_total : 0
        return Math.max(0, Math.min(1, Math.max(files, bytes)))
    }
    readonly property bool selectedJobFailed: controller.status.startsWith("Analysis failed:")
        || controller.status.startsWith("Defragmentation failed:")
    onSelectedVolumeIdChanged: controller.select_volume(selectedVolumeId)
    function bytes(value) {
        if (!value) return "0 B"
        const units = ["B", "KiB", "MiB", "GiB", "TiB"]
        let unit = 0
        let number = value
        while (number >= 1024 && unit < units.length - 1) { number /= 1024; ++unit }
        return number.toFixed(unit === 0 ? 0 : 1) + " " + units[unit]
    }
    function percent(basisPoints) {
        return basisPoints === undefined || basisPoints === null || basisPoints < 0
            ? "—" : (basisPoints / 100).toFixed(1) + "%"
    }
    function integer(value) {
        if (!isFinite(value) || value < 0)
            return "0"
        return Math.floor(value).toLocaleString(Qt.locale(), "f", 0)
    }
    Controller {
        id: controller
        onVolume_countChanged: {
            if (window.selectedIndex < 0 && volume_count > 0)
                window.selectedIndex = 0
            else if (window.selectedIndex >= volume_count)
                window.selectedIndex = volume_count - 1
        }
        onPlan_revisionChanged: {
            if (plan_revision > 0) {
                if (plan_available) {
                    planWindow.show()
                    planWindow.raise()
                    planWindow.requestActivate()
                } else {
                    planUnavailableDialog.targetVolumeId = window.selectedVolumeId
                    planUnavailableDialog.requiresUnmount = window.selectedRequiresUnmount
                    planUnavailableDialog.open()
                }
            }
        }
    }

    Component.onCompleted: controller.refresh()

    menuBar: Controls.MenuBar {
        id: appMenu
        Controls.Menu {
            title: qsTr("Action")
            Controls.MenuItem { text: qsTr("Refresh volumes"); onTriggered: controller.refresh() }
            Controls.MenuItem { text: qsTr("Analyze"); enabled: window.selectedIndex >= 0 && !controller.busy; onTriggered: analyzeSelected() }
            Controls.MenuSeparator {}
            Controls.MenuItem { text: qsTr("Quit"); onTriggered: Qt.quit() }
        }
        Controls.Menu {
            title: qsTr("Settings")
            Controls.MenuItem { text: qsTr("Analysis settings…"); enabled: false }
        }
        Controls.Menu {
            title: qsTr("Help")
            Controls.MenuItem { text: qsTr("About Defragger"); onTriggered: aboutDialog.open() }
        }
    }

    function analyzeSelected() {
        if (hasSelectedVolume)
            controller.analyze(selectedVolumeId)
    }

    ColumnLayout {
        anchors.fill: parent
        anchors.topMargin: appMenu.height
        anchors.margins: Kirigami.Units.smallSpacing
        spacing: Kirigami.Units.smallSpacing

        Rectangle {
            id: volumeTable
            Layout.fillWidth: true
            Layout.preferredHeight: 224
            color: Kirigami.Theme.backgroundColor
            border.color: Kirigami.Theme.disabledTextColor
            border.width: 1

            readonly property real tableContentWidth: Math.max(960, width - 2)
            readonly property real extraColumnWidth: Math.max(0, tableContentWidth - 960)
            readonly property real mountPointColumnWidth: 116
                + Math.min(184, extraColumnWidth * 0.55)
            readonly property real deviceColumnWidth: 140
                + Math.min(140, extraColumnWidth * 0.45)
            readonly property real deviceColumnX: 10
            readonly property real mountPointColumnX: deviceColumnX
                + deviceColumnWidth + 6
            readonly property real filesystemColumnX: mountPointColumnX
                + mountPointColumnWidth + 6
            readonly property real sizeColumnX: filesystemColumnX + 62
            readonly property real usedColumnX: sizeColumnX + 74
            readonly property real freeColumnX: usedColumnX + 74
            readonly property real usageColumnX: freeColumnX + 74
            readonly property real fragmentedColumnX: usageColumnX + 106
            readonly property real analysisColumnX: fragmentedColumnX + 84

            ColumnLayout {
                anchors.fill: parent
                anchors.margins: 1
                spacing: 0

                Item {
                    Layout.fillWidth: true
                    Layout.preferredHeight: 28
                    clip: true

                    Rectangle {
                        anchors.fill: parent
                        color: Kirigami.Theme.alternateBackgroundColor
                    }

                    Item {
                        x: -volumeList.contentX
                        width: volumeTable.tableContentWidth
                        height: parent.height

                        Controls.Label { x: volumeTable.deviceColumnX; width: volumeTable.deviceColumnWidth; height: parent.height; text: qsTr("Device"); font.bold: true; verticalAlignment: Text.AlignVCenter }
                        Controls.Label { x: volumeTable.mountPointColumnX; width: volumeTable.mountPointColumnWidth; height: parent.height; text: qsTr("Mount point"); font.bold: true; verticalAlignment: Text.AlignVCenter }
                        Controls.Label { x: volumeTable.filesystemColumnX; width: 56; height: parent.height; text: qsTr("FS"); font.bold: true; verticalAlignment: Text.AlignVCenter }
                        Controls.Label { x: volumeTable.sizeColumnX; width: 68; height: parent.height; text: qsTr("Size"); font.bold: true; horizontalAlignment: Text.AlignRight; verticalAlignment: Text.AlignVCenter }
                        Controls.Label { x: volumeTable.usedColumnX; width: 68; height: parent.height; text: qsTr("Used"); font.bold: true; horizontalAlignment: Text.AlignRight; verticalAlignment: Text.AlignVCenter }
                        Controls.Label { x: volumeTable.freeColumnX; width: 68; height: parent.height; text: qsTr("Free"); font.bold: true; horizontalAlignment: Text.AlignRight; verticalAlignment: Text.AlignVCenter }
                        Controls.Label { x: volumeTable.usageColumnX; width: 100; height: parent.height; text: qsTr("Usage"); font.bold: true; horizontalAlignment: Text.AlignHCenter; verticalAlignment: Text.AlignVCenter }
                        Controls.Label { x: volumeTable.fragmentedColumnX; width: 78; height: parent.height; text: qsTr("Frag."); font.bold: true; horizontalAlignment: Text.AlignRight; verticalAlignment: Text.AlignVCenter }
                        Controls.Label { x: volumeTable.analysisColumnX; width: parent.width - x - 10; height: parent.height; text: qsTr("Analysis"); font.bold: true; verticalAlignment: Text.AlignVCenter }
                    }
                }

                ListView {
                    id: volumeList
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    clip: true
                    model: controller.volume_count
                    contentWidth: volumeTable.tableContentWidth
                    boundsBehavior: Flickable.StopAtBounds
                    Controls.ScrollBar.vertical: Controls.ScrollBar {
                        policy: Controls.ScrollBar.AlwaysOn
                    }
                    Controls.ScrollBar.horizontal: Controls.ScrollBar {
                        policy: Controls.ScrollBar.AsNeeded
                    }
                    delegate: Rectangle {
                        required property int index
                        readonly property int volumeRevision: controller.volume_revision
                        readonly property string volumeId: {
                            const revision = volumeRevision
                            return controller.volume_id(index)
                        }
                        readonly property string mountPoint: {
                            const revision = volumeRevision
                            return controller.volume_mount_point(index)
                        }
                        readonly property string source: {
                            const revision = volumeRevision
                            return controller.volume_source(index)
                        }
                        readonly property string filesystem: {
                            const revision = volumeRevision
                            return controller.volume_filesystem(index)
                        }
                        readonly property int statsRevision: controller.analysis_revision
                            + volumeRevision
                        readonly property double capacityBytes: {
                            const revision = statsRevision
                            return controller.volume_capacity_bytes(index)
                        }
                        readonly property double usedBytes: {
                            const revision = statsRevision
                            return controller.volume_used_bytes(index)
                        }
                        width: volumeTable.tableContentWidth
                        height: 38
                        color: window.selectedIndex === index ? Kirigami.Theme.highlightColor : (index % 2 ? Kirigami.Theme.alternateBackgroundColor : Kirigami.Theme.backgroundColor)
                        Controls.Label {
                            x: volumeTable.deviceColumnX
                            width: volumeTable.deviceColumnWidth
                            height: parent.height
                            text: source
                            elide: Text.ElideMiddle
                            verticalAlignment: Text.AlignVCenter
                        }
                        Controls.Label {
                            x: volumeTable.mountPointColumnX
                            width: volumeTable.mountPointColumnWidth
                            height: parent.height
                            text: mountPoint.length > 0 ? mountPoint : "—"
                            elide: Text.ElideMiddle
                            verticalAlignment: Text.AlignVCenter
                        }
                        Controls.Label {
                            x: volumeTable.filesystemColumnX
                            width: 56
                            height: parent.height
                            text: filesystem
                            elide: Text.ElideRight
                            verticalAlignment: Text.AlignVCenter
                        }
                        Controls.Label {
                            x: volumeTable.sizeColumnX
                            width: 68
                            height: parent.height
                            text: window.bytes(capacityBytes)
                            horizontalAlignment: Text.AlignRight
                            verticalAlignment: Text.AlignVCenter
                        }
                        Controls.Label {
                            x: volumeTable.usedColumnX
                            width: 68
                            height: parent.height
                            text: window.bytes(usedBytes)
                            horizontalAlignment: Text.AlignRight
                            verticalAlignment: Text.AlignVCenter
                        }
                        Controls.Label {
                            x: volumeTable.freeColumnX
                            width: 68
                            height: parent.height
                            text: {
                                const revision = statsRevision
                                return window.bytes(controller.volume_free_bytes(index))
                            }
                            horizontalAlignment: Text.AlignRight
                            verticalAlignment: Text.AlignVCenter
                        }
                        Item {
                            x: volumeTable.usageColumnX
                            width: 100
                            height: 16
                            anchors.verticalCenter: parent.verticalCenter
                            Rectangle {
                                anchors.fill: parent
                                anchors.topMargin: 2; anchors.bottomMargin: 2
                                radius: 5
                                color: Kirigami.Theme.alternateBackgroundColor
                                border.color: Kirigami.Theme.disabledTextColor
                                Rectangle {
                                    height: parent.height
                                    width: parent.width * (capacityBytes > 0 ? usedBytes / capacityBytes : 0)
                                    radius: parent.radius
                                    color: Kirigami.Theme.highlightColor
                                }
                            }
                            Controls.Label {
                                anchors.centerIn: parent
                                text: capacityBytes > 0 ? Math.round(usedBytes * 100 / capacityBytes) + "%" : "—"
                                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            }
                        }
                        Controls.Label {
                            x: volumeTable.fragmentedColumnX
                            width: 78
                            height: parent.height
                            text: {
                                const revision = controller.analysis_revision
                                return controller.volume_has_report(index)
                                    ? window.percent(controller.volume_fragmented_basis_points(index)) : "—"
                            }
                            horizontalAlignment: Text.AlignRight
                            verticalAlignment: Text.AlignVCenter
                        }
                        Controls.Label {
                            x: volumeTable.analysisColumnX
                            width: parent.width - x - 10
                            height: parent.height
                            text: {
                                const revision = controller.analysis_revision
                                return controller.busy && String(controller.analyzing_volume_id) === volumeId
                                    ? (controller.active_operation === "compaction"
                                        ? qsTr("Compacting…")
                                        : (controller.active_operation === "defragmentation"
                                            ? qsTr("Defragmenting…") : qsTr("Analyzing…")))
                                    : (controller.volume_has_report(index)
                                        ? qsTr("Analyzed")
                                        : (controller.volume_supported(index) ? "" : qsTr("Unsupported")))
                            }
                            elide: Text.ElideRight
                            verticalAlignment: Text.AlignVCenter
                        }
                        MouseArea { anchors.fill: parent; onClicked: window.selectedIndex = index; onDoubleClicked: window.analyzeSelected() }
                    }
                    Kirigami.PlaceholderMessage {
                        anchors.centerIn: parent
                        visible: controller.volume_count === 0
                        text: controller.status.length > 0
                            ? controller.status : qsTr("No disk volumes found")
                    }
                }
            }

        }

        DriveMap {
            Layout.fillWidth: true
            Layout.fillHeight: true
            Layout.minimumHeight: 100
            capacityBytes: window.hasSelectedVolume
                ? controller.volume_capacity_bytes(window.selectedIndex) : 0
            volumeId: window.selectedVolumeId
            useAnalysis: window.hasSelectedVolume
                && window.selectedVolumeId === String(controller.map_volume_id)
            sourceRevision: useAnalysis ? controller.map_revision : 0
            mapData: controller.display_map_data
            activityData: controller.activity_data
            activityRevision: controller.activity_revision
            detailsProvider: controller
            renderedGeneration: controller.display_map_generation
            onRebuildRequested: function(width, height, capacity, analysis, generation) {
                controller.render_map(width, height, capacity, analysis, generation)
            }
        }

        Controls.TabBar {
            id: tabs
            Layout.fillWidth: true
            Controls.TabButton { text: qsTr("Drive") }
            Controls.TabButton { text: qsTr("File list") }
        }

        StackLayout {
            Layout.fillWidth: true
            Layout.preferredHeight: 140
            Layout.minimumHeight: 140
            Layout.maximumHeight: 140
            currentIndex: tabs.currentIndex

            Kirigami.ShadowedRectangle {
                color: Kirigami.Theme.backgroundColor; border.color: Kirigami.Theme.disabledTextColor; border.width: 1
                ColumnLayout {
                    anchors.fill: parent; anchors.margins: Kirigami.Units.largeSpacing
                    RowLayout {
                        Layout.fillWidth: true
                        Controls.Label {
                            text: controller.volume_count === 0
                                && controller.status.length > 0
                                ? qsTr("Unavailable")
                                : (window.selectedJobFailed
                                    ? qsTr("Operation failed")
                                    : (window.selectedIsBeingOptimized
                                    ? (controller.active_operation === "compaction"
                                        ? qsTr("Compaction in progress")
                                        : qsTr("Defragmentation in progress"))
                                    : (window.selectedIsBeingAnalyzed
                                    ? qsTr("Analysis in progress")
                                    : (window.selectedHasReport
                                        ? qsTr("Analysis complete") : qsTr("Ready")))))
                            font.bold: true
                        }
                        Controls.Label {
                            Layout.fillWidth: true
                            visible: controller.status.length > 0
                            text: controller.status
                            elide: Text.ElideMiddle
                            horizontalAlignment: Text.AlignRight
                            color: Kirigami.Theme.disabledTextColor
                        }
                    }
                    ColumnLayout {
                        Layout.fillWidth: true
                        visible: window.selectedIsBeingOptimized
                        spacing: 2

                        Rectangle {
                            Layout.fillWidth: true
                            Layout.preferredHeight: 10
                            radius: height / 2
                            color: Kirigami.Theme.alternateBackgroundColor
                            border.width: 1
                            border.color: Kirigami.Theme.disabledTextColor
                            clip: true

                            Rectangle {
                                anchors.left: parent.left
                                anchors.top: parent.top
                                anchors.bottom: parent.bottom
                                width: parent.width * window.optimizationProgress
                                radius: parent.radius
                                color: Kirigami.Theme.highlightColor

                                Behavior on width {
                                    NumberAnimation {
                                        duration: Kirigami.Units.shortDuration
                                        easing.type: Easing.OutCubic
                                    }
                                }
                            }
                        }
                        RowLayout {
                            Layout.fillWidth: true
                            Controls.Label {
                                text: qsTr("%1 of %2 files")
                                    .arg(window.integer(controller.defrag_files_completed))
                                    .arg(window.integer(controller.defrag_files_total))
                                color: Kirigami.Theme.disabledTextColor
                                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            }
                            Controls.Label {
                                Layout.fillWidth: true
                                text: qsTr("%1 of %2 moved · %3%")
                                    .arg(window.bytes(controller.defrag_bytes_moved))
                                    .arg(window.bytes(controller.defrag_bytes_total))
                                    .arg(Math.floor(window.optimizationProgress * 100))
                                horizontalAlignment: Text.AlignRight
                                color: Kirigami.Theme.disabledTextColor
                                font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                            }
                        }
                    }
                    RowLayout {
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        spacing: Kirigami.Units.largeSpacing
                        ColumnLayout {
                            Layout.fillWidth: true
                            Controls.Label { text: qsTr("Files scanned"); color: Kirigami.Theme.disabledTextColor; font.pixelSize: Kirigami.Theme.smallFont.pixelSize }
                            Controls.Label { text: window.integer(window.selectedHasReport || window.selectedIsActive ? controller.files_scanned : 0); font.bold: true }
                        }
                        ColumnLayout {
                            Layout.fillWidth: true
                            Controls.Label { text: qsTr("Allocated data"); color: Kirigami.Theme.disabledTextColor; font.pixelSize: Kirigami.Theme.smallFont.pixelSize }
                            Controls.Label { text: window.bytes(window.selectedHasReport || window.selectedIsActive ? controller.bytes_scanned : 0); font.bold: true }
                        }
                        ColumnLayout {
                            Layout.fillWidth: true
                            Controls.Label { text: qsTr("Coverage"); color: Kirigami.Theme.disabledTextColor; font.pixelSize: Kirigami.Theme.smallFont.pixelSize }
                            Controls.Label { text: window.selectedHasReport ? window.percent(controller.coverage_basis_points) : "—"; font.bold: true }
                        }
                        ColumnLayout {
                            Layout.fillWidth: true
                            Controls.Label { text: qsTr("Skipped"); color: Kirigami.Theme.disabledTextColor; font.pixelSize: Kirigami.Theme.smallFont.pixelSize }
                            Controls.Label { text: window.selectedHasReport ? window.integer(controller.skipped_entries) : "—"; font.bold: true }
                        }
                        ColumnLayout {
                            Layout.fillWidth: true
                            Controls.Label { text: qsTr("Fragmented data"); color: Kirigami.Theme.disabledTextColor; font.pixelSize: Kirigami.Theme.smallFont.pixelSize }
                            Controls.Label { text: window.selectedHasReport ? window.percent(controller.fragmented_basis_points) : qsTr("Not analyzed"); font.bold: true }
                        }
                    }
                }
            }

            ColumnLayout {
                id: fileTable
                readonly property int sizeColumnWidth: 100
                readonly property int fragmentColumnWidth: 100
                readonly property int averageColumnWidth: 180
                readonly property int columnSpacing: Kirigami.Units.largeSpacing * 2
                spacing: 0

                Rectangle {
                    Layout.fillWidth: true
                    Layout.preferredHeight: 28
                    color: Kirigami.Theme.alternateBackgroundColor

                    RowLayout {
                        anchors.fill: parent
                        anchors.leftMargin: 12
                        anchors.rightMargin: 12
                        spacing: fileTable.columnSpacing

                        Controls.Label {
                            Layout.fillWidth: true
                            text: qsTr("File")
                            font.bold: true
                        }
                        Controls.Label {
                            Layout.preferredWidth: fileTable.sizeColumnWidth
                            text: qsTr("Size")
                            font.bold: true
                            horizontalAlignment: Text.AlignRight
                        }
                        Controls.Label {
                            Layout.preferredWidth: fileTable.fragmentColumnWidth
                            text: qsTr("Fragments")
                            font.bold: true
                            horizontalAlignment: Text.AlignRight
                        }
                        Controls.Label {
                            Layout.preferredWidth: fileTable.averageColumnWidth
                            text: qsTr("Fragment size (avg)")
                            font.bold: true
                            horizontalAlignment: Text.AlignRight
                        }
                    }

                    Rectangle {
                        anchors.left: parent.left
                        anchors.right: parent.right
                        anchors.bottom: parent.bottom
                        height: 1
                        color: Kirigami.Theme.disabledTextColor
                    }
                }

                ListView {
                    id: fileList
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    clip: true
                    model: window.selectedHasReport ? controller.file_row_count : 0
                    delegate: Controls.ItemDelegate {
                        required property int index
                        width: ListView.view.width

                        contentItem: RowLayout {
                            spacing: fileTable.columnSpacing
                            Controls.Label {
                                Layout.fillWidth: true
                                text: controller.file_path(index)
                                elide: Text.ElideMiddle
                            }
                            Controls.Label {
                                Layout.preferredWidth: fileTable.sizeColumnWidth
                                text: window.bytes(controller.file_size_bytes(index))
                                horizontalAlignment: Text.AlignRight
                            }
                            Controls.Label {
                                Layout.preferredWidth: fileTable.fragmentColumnWidth
                                text: window.integer(controller.file_fragment_count(index))
                                horizontalAlignment: Text.AlignRight
                            }
                            Controls.Label {
                                Layout.preferredWidth: fileTable.averageColumnWidth
                                text: window.bytes(controller.file_average_fragment_bytes(index))
                                horizontalAlignment: Text.AlignRight
                            }
                        }
                    }
                    Kirigami.PlaceholderMessage {
                        anchors.centerIn: parent
                        visible: fileList.count === 0
                        text: qsTr("Analyze a volume to inspect fragmented files")
                    }
                }
            }
        }

        RowLayout {
            Layout.fillWidth: true
            Controls.Button { text: qsTr("Analyze"); icon.name: "system-search"; enabled: window.selectedIndex >= 0 && !controller.busy; onClicked: window.analyzeSelected() }
            Controls.Button { text: qsTr("Defragment…"); icon.name: "drive-harddisk"; enabled: !controller.busy && window.selectedHasReport; onClicked: controller.build_plan() }
            Controls.Button { text: qsTr("Compact…"); icon.name: "transform-move"; enabled: !controller.busy && window.selectedHasReport && (window.selectedRequiresUnmount || controller.volume_can_compact(window.selectedIndex)); onClicked: controller.build_compact_plan() }
            Item { Layout.preferredWidth: Kirigami.Units.largeSpacing }
            Controls.Button { text: controller.paused ? qsTr("Resume") : qsTr("Pause"); enabled: controller.busy && controller.active_operation !== "unmount"; onClicked: controller.paused ? controller.resume() : controller.pause() }
            Controls.Button { text: qsTr("Stop"); icon.name: "process-stop"; enabled: controller.busy && controller.active_operation !== "unmount"; onClicked: controller.stop() }
            Item { Layout.fillWidth: true }
            Controls.BusyIndicator { running: controller.busy; visible: running; Layout.preferredWidth: 26; Layout.preferredHeight: 26 }
        }
    }

    Window {
        id: planWindow
        width: 920
        height: 620
        minimumWidth: 680
        minimumHeight: 420
        transientParent: window
        modality: Qt.WindowModal
        flags: Qt.Dialog
        color: Kirigami.Theme.backgroundColor
        visible: false
        title: controller.plan_is_compact ? qsTr("Compaction plan preview") : qsTr("Defragmentation plan preview")

        Shortcut { sequences: [StandardKey.Cancel]; onActivated: planWindow.close() }

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: Kirigami.Units.largeSpacing
            spacing: Kirigami.Units.smallSpacing

            Controls.Label {
                Layout.fillWidth: true
                text: controller.plan_message.length > 0
                    ? controller.plan_message
                    : (controller.plan_is_compact
                        ? qsTr("Compaction may move already-contiguous supporting files. The volume must remain unmounted.")
                        : qsTr("Each file is revalidated immediately before it is moved."))
                font.bold: true
                color: Kirigami.Theme.neutralTextColor
                wrapMode: Text.Wrap
            }
            Controls.Label {
                Layout.fillWidth: true
                text: qsTr("%1 files · %2 → %3 fragments · %4 estimated rewrite")
                    .arg(window.integer(controller.plan_candidate_count))
                    .arg(window.integer(controller.plan_current_fragment_count))
                    .arg(window.integer(controller.plan_target_fragment_count))
                    .arg(window.bytes(controller.plan_estimated_rewrite_bytes))
                wrapMode: Text.Wrap
            }

            Rectangle {
                id: planHeader
                readonly property int fragmentColumnWidth: 105
                readonly property int roleColumnWidth: controller.plan_is_compact ? 135 : 0
                readonly property int columnSpacing: Kirigami.Units.largeSpacing
                Layout.fillWidth: true
                Layout.preferredHeight: 34
                color: Kirigami.Theme.alternateBackgroundColor

                RowLayout {
                    anchors.fill: parent
                    anchors.leftMargin: 12
                    anchors.rightMargin: 12
                    spacing: planHeader.columnSpacing
                    Controls.Label { Layout.fillWidth: true; text: qsTr("File (relative to filesystem root)"); font.bold: true }
                    Controls.Label { Layout.preferredWidth: planHeader.fragmentColumnWidth; text: qsTr("Fragments now"); font.bold: true; horizontalAlignment: Text.AlignRight }
                    Controls.Label { Layout.preferredWidth: planHeader.fragmentColumnWidth; text: qsTr("After"); font.bold: true; horizontalAlignment: Text.AlignRight }
                    Controls.Label { Layout.preferredWidth: planHeader.roleColumnWidth; visible: controller.plan_is_compact; text: qsTr("Role"); font.bold: true }
                }
            }
            ListView {
                Layout.fillWidth: true; Layout.fillHeight: true; clip: true
                model: controller.plan_candidate_count
                delegate: Controls.ItemDelegate {
                    required property int index
                    width: ListView.view.width
                    readonly property string candidatePath: controller.plan_candidate_path(index)
                    Controls.ToolTip.visible: hovered
                    Controls.ToolTip.text: candidatePath
                    contentItem: RowLayout {
                        spacing: planHeader.columnSpacing
                        Controls.Label { Layout.fillWidth: true; text: candidatePath; elide: Text.ElideMiddle }
                        Controls.Label { Layout.preferredWidth: planHeader.fragmentColumnWidth; text: window.integer(controller.plan_candidate_current_runs(index)); horizontalAlignment: Text.AlignRight }
                        Controls.Label { Layout.preferredWidth: planHeader.fragmentColumnWidth; text: window.integer(controller.plan_candidate_target_runs(index)); horizontalAlignment: Text.AlignRight }
                        Controls.Label {
                            Layout.preferredWidth: planHeader.roleColumnWidth
                            visible: controller.plan_is_compact
                            text: controller.plan_candidate_is_support(index) ? qsTr("Supporting move") : qsTr("Defragment")
                        }
                    }
                }
                Kirigami.PlaceholderMessage {
                    anchors.centerIn: parent
                    visible: controller.plan_candidate_count === 0
                    text: qsTr("No eligible fragmented files were found.")
                }
            }
            RowLayout {
                Layout.fillWidth: true
                Item { Layout.fillWidth: true }
                Controls.Button { text: qsTr("Close"); onClicked: planWindow.close() }
                Controls.Button {
                    text: controller.plan_is_compact ? qsTr("Start compaction") : qsTr("Start defragmentation")
                    icon.name: "media-playback-start"
                    enabled: controller.plan_available
                        && controller.plan_candidate_count > 0 && !controller.busy
                    onClicked: {
                        planWindow.close()
                        controller.start_defrag()
                    }
                }
            }
        }
    }

    Controls.Dialog {
        id: planUnavailableDialog
        property string targetVolumeId: ""
        property bool requiresUnmount: false
        anchors.centerIn: parent
        modal: true
        title: controller.plan_is_compact
            ? qsTr("Cannot compact this volume")
            : qsTr("Cannot defragment this volume")
        width: Math.min(560, window.width - 2 * Kirigami.Units.largeSpacing)
        contentItem: Controls.Label {
            text: (controller.plan_message.length > 0
                ? controller.plan_message
                : qsTr("Optimization is unavailable for this volume."))
                + (planUnavailableDialog.requiresUnmount
                    ? qsTr("\n\nUnmount it now and analyze it again? Open files may prevent a normal unmount.")
                    : "")
            wrapMode: Text.Wrap
            width: planUnavailableDialog.availableWidth
        }
        footer: Controls.DialogButtonBox {
            Controls.Button {
                visible: planUnavailableDialog.requiresUnmount
                text: qsTr("Cancel")
                Controls.DialogButtonBox.buttonRole: Controls.DialogButtonBox.RejectRole
            }
            Controls.Button {
                text: planUnavailableDialog.requiresUnmount
                    ? qsTr("Unmount and analyze again") : qsTr("OK")
                Controls.DialogButtonBox.buttonRole: Controls.DialogButtonBox.AcceptRole
            }
            onAccepted: {
                planUnavailableDialog.close()
                if (planUnavailableDialog.requiresUnmount)
                    controller.unmount_and_analyze(planUnavailableDialog.targetVolumeId)
            }
            onRejected: planUnavailableDialog.close()
        }
    }

    Controls.Dialog {
        id: aboutDialog
        anchors.centerIn: parent
        modal: true
        title: qsTr("About Defragger")
        standardButtons: Controls.Dialog.Ok
        Controls.Label { text: qsTr("A Plasma-native, direct-kernel ext4 analysis prototype written in Rust."); wrapMode: Text.Wrap; width: 420 }
    }
}
