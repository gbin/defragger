import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import io.github.defragger

Kirigami.ApplicationWindow {
    id: window
    width: 1000
    height: 720
    minimumWidth: 760
    minimumHeight: 560
    visible: true
    title: qsTr("Defragger")

    property var volumes: []
    property var report: ({})
    property var plan: ({})
    property int selectedIndex: -1
    property string mapVolumeId: ""
    readonly property var selectedVolume: selectedIndex >= 0 && selectedIndex < volumes.length
        ? volumes[selectedIndex] : null

    function parseOr(value, fallback) {
        try { return JSON.parse(value) } catch (_) { return fallback }
    }
    function bytes(value) {
        if (!value) return "0 B"
        const units = ["B", "KiB", "MiB", "GiB", "TiB"]
        let unit = 0
        let number = value
        while (number >= 1024 && unit < units.length - 1) { number /= 1024; ++unit }
        return number.toFixed(unit === 0 ? 0 : 1) + " " + units[unit]
    }
    function percent(basisPoints) {
        return basisPoints === undefined || basisPoints === null ? "—" : (basisPoints / 100).toFixed(1) + "%"
    }
    function supportText(support) {
        return support === "ReadOnly" ? qsTr("Analyze") : qsTr("Unsupported")
    }

    Controller {
        id: controller
        onVolumes_jsonChanged: {
            window.volumes = window.parseOr(volumes_json, [])
            if (window.selectedIndex < 0 && window.volumes.length > 0)
                window.selectedIndex = 0
        }
        onReport_jsonChanged: window.report = window.parseOr(report_json, ({}))
        onPlan_jsonChanged: {
            window.plan = window.parseOr(plan_json, ({}))
            if (window.plan.candidates !== undefined)
                planDialog.open()
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
        if (selectedVolume) {
            mapVolumeId = String(selectedVolume.id)
            controller.analyze(mapVolumeId)
        }
    }

    ColumnLayout {
        anchors.fill: parent
        anchors.topMargin: appMenu.height
        anchors.margins: Kirigami.Units.smallSpacing
        spacing: Kirigami.Units.smallSpacing

        Rectangle {
            Layout.fillWidth: true
            Layout.preferredHeight: 190
            color: Kirigami.Theme.backgroundColor
            border.color: Kirigami.Theme.disabledTextColor
            border.width: 1

            Item {
                anchors.fill: parent
                ListView {
                    id: volumeList
                    anchors.fill: parent
                    anchors.margins: 1
                    clip: true
                    model: window.volumes
                    delegate: Rectangle {
                        required property var modelData
                        required property int index
                        width: volumeList.width
                        height: 38
                        color: window.selectedIndex === index ? Kirigami.Theme.highlightColor : (index % 2 ? Kirigami.Theme.alternateBackgroundColor : Kirigami.Theme.backgroundColor)
                        RowLayout {
                            anchors.fill: parent
                            anchors.leftMargin: 10; anchors.rightMargin: 10
                            spacing: 8
                            Kirigami.Icon { source: "drive-harddisk"; Layout.preferredWidth: 20; Layout.preferredHeight: 20 }
                            Controls.Label { text: modelData.mount_point + "  (" + modelData.source + ")"; elide: Text.ElideMiddle; Layout.preferredWidth: 200 }
                            Controls.Label { text: modelData.filesystem; Layout.preferredWidth: 80 }
                            Controls.Label { text: window.bytes(modelData.capacity_bytes); Layout.preferredWidth: 85; horizontalAlignment: Text.AlignRight }
                            Controls.Label { text: window.bytes(modelData.used_bytes); Layout.preferredWidth: 85; horizontalAlignment: Text.AlignRight }
                            Controls.Label { text: window.bytes(modelData.free_bytes); Layout.preferredWidth: 85; horizontalAlignment: Text.AlignRight }
                            Item {
                                Layout.preferredWidth: 120
                                Layout.preferredHeight: 16
                                Rectangle {
                                    anchors.fill: parent
                                    anchors.topMargin: 2; anchors.bottomMargin: 2
                                    radius: 5
                                    color: Kirigami.Theme.alternateBackgroundColor
                                    border.color: Kirigami.Theme.disabledTextColor
                                    Rectangle {
                                        height: parent.height
                                        width: parent.width * (modelData.capacity_bytes > 0 ? modelData.used_bytes / modelData.capacity_bytes : 0)
                                        radius: parent.radius
                                        color: Kirigami.Theme.highlightColor
                                    }
                                }
                                Controls.Label {
                                    anchors.centerIn: parent
                                    text: modelData.capacity_bytes > 0 ? Math.round(modelData.used_bytes * 100 / modelData.capacity_bytes) + "%" : "—"
                                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                                }
                            }
                            Controls.Label {
                                text: window.report.volume && window.report.volume.id === modelData.id ? window.percent(window.report.fragmentation.fragmented_basis_points) : "—"
                                Layout.preferredWidth: 100; horizontalAlignment: Text.AlignRight
                            }
                            Controls.Label { text: controller.busy && window.selectedIndex === index ? qsTr("Analyzing…") : window.supportText(modelData.support); Layout.fillWidth: true }
                        }
                        MouseArea { anchors.fill: parent; onClicked: window.selectedIndex = index; onDoubleClicked: window.analyzeSelected() }
                    }
                    Kirigami.PlaceholderMessage { anchors.centerIn: parent; visible: volumeList.count === 0; text: qsTr("No disk volumes found") }
                }
            }
        }

        DriveMap {
            Layout.fillWidth: true
            Layout.fillHeight: true
            Layout.minimumHeight: 100
            capacityBytes: window.selectedVolume ? window.selectedVolume.capacity_bytes : 0
            useAnalysis: window.selectedVolume
                && String(window.selectedVolume.id) === window.mapVolumeId
            sourceRevision: useAnalysis ? controller.map_revision : 0
            mapJson: controller.display_map_json
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
                    Controls.Label { text: controller.busy ? qsTr("Analysis in progress") : (window.report.volume ? qsTr("Analysis complete") : qsTr("Ready")); font.bold: true }
                    Controls.Label {
                        visible: text.length > 0
                        text: controller.status
                        elide: Text.ElideMiddle
                        Layout.fillWidth: true
                    }
                    RowLayout {
                        Controls.Label { text: qsTr("Files scanned: %1").arg(Math.floor(controller.files_scanned).toLocaleString()) }
                        Controls.Label { text: qsTr("Allocated data scanned: %1").arg(window.bytes(controller.bytes_scanned)) }
                        Controls.Label { visible: window.report.coverage !== undefined; text: qsTr("Coverage: %1 · %2 skipped").arg(window.report.coverage ? window.percent(window.report.coverage.estimated_basis_points) : "—").arg(window.report.coverage ? window.report.coverage.skipped_entries : 0) }
                        Item { Layout.fillWidth: true }
                        Controls.Label { text: window.report.fragmentation ? qsTr("Fragmented scanned data: %1").arg(window.percent(window.report.fragmentation.fragmented_basis_points)) : qsTr("Fragmentation: not analyzed") }
                    }
                }
            }

            ListView {
                clip: true
                model: window.report.files || []
                delegate: Controls.ItemDelegate {
                    required property var modelData
                    width: ListView.view.width
                    text: modelData.path + "    " + qsTr("%1 runs (%2 excess)").arg(modelData.physical_runs).arg(modelData.excess_runs)
                }
                Kirigami.PlaceholderMessage { anchors.centerIn: parent; visible: parent.count === 0; text: qsTr("Analyze a volume to inspect fragmented files") }
            }
        }

        RowLayout {
            Layout.fillWidth: true
            Controls.Button { text: qsTr("Analyze"); icon.name: "system-search"; enabled: window.selectedIndex >= 0 && !controller.busy; onClicked: window.analyzeSelected() }
            Controls.Button { text: qsTr("Defragment…"); icon.name: "drive-harddisk"; enabled: !controller.busy && window.report.volume !== undefined; onClicked: controller.build_plan() }
            Item { Layout.preferredWidth: Kirigami.Units.largeSpacing }
            Controls.Button { text: controller.paused ? qsTr("Resume") : qsTr("Pause"); enabled: controller.busy; onClicked: controller.paused ? controller.resume() : controller.pause() }
            Controls.Button { text: qsTr("Stop"); icon.name: "process-stop"; enabled: controller.busy; onClicked: controller.stop() }
            Item { Layout.fillWidth: true }
            Controls.BusyIndicator { running: controller.busy; visible: running; Layout.preferredWidth: 26; Layout.preferredHeight: 26 }
        }
    }

    Controls.Dialog {
        id: planDialog
        anchors.centerIn: parent
        width: Math.min(window.width - 80, 720)
        height: Math.min(window.height - 80, 520)
        modal: true
        title: qsTr("Defragmentation plan preview")
        standardButtons: Controls.Dialog.Close
        ColumnLayout {
            anchors.fill: parent
            Controls.Label { text: qsTr("Read-only v0 — no extents will be moved"); font.bold: true; color: Kirigami.Theme.neutralTextColor }
            Controls.Label { text: qsTr("%1 candidate files · %2 estimated rewrite").arg(window.plan.candidates ? window.plan.candidates.length : 0).arg(window.bytes(window.plan.estimated_rewrite_bytes)); wrapMode: Text.Wrap }
            ListView {
                Layout.fillWidth: true; Layout.fillHeight: true; clip: true
                model: window.plan.candidates || []
                delegate: Controls.ItemDelegate { required property var modelData; width: ListView.view.width; text: modelData.path + "  " + modelData.current_runs + " → " + modelData.target_runs + " runs" }
            }
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
