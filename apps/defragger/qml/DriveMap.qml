import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

Item {
    id: root

    property var mapData
    property var pendingIoData
    property int pendingIoRevision: 0
    property var detailsProvider: null
    property string volumeId: ""
    property double capacityBytes: 0
    property bool useAnalysis: false
    property int sourceRevision: 0
    property int renderedGeneration: 0
    property var mapView: null
    property var pendingIoView: null
    property int binCount: 0
    property int hoveredIndex: -1
    property bool workerBusy: false
    property bool rebuildPending: false
    property bool geometryPending: false
    property int rebuildGeneration: 0
    readonly property int recordBytes: 74
    readonly property int contributorOffset: 44
    readonly property int maxContributors: 5
    readonly property color readActivityColor: "#4fc3f7"
    readonly property color writeActivityColor: "#ffd166"

    signal rebuildRequested(
        real width,
        real height,
        real capacityBytes,
        bool useAnalysis,
        int generation
    )

    readonly property var metadataTypes: [
        ["filesystem_headers", qsTr("FS headers"), "#8e62d9"],
        ["journal", qsTr("Journal / log"), "#e89b42"],
        ["allocation_tables", qsTr("Allocation tables"), "#26a69a"],
        ["file_metadata", qsTr("File metadata"), "#6c78d8"],
        ["group_descriptors", qsTr("Group descriptors"), "#d4b33f"],
        ["block_bitmaps", qsTr("Block bitmaps"), "#38b9c7"],
        ["file_bitmaps", qsTr("File bitmaps"), "#d56da1"],
        ["reserved", qsTr("Reserved metadata"), "#9b7653"],
        ["other", qsTr("Other metadata"), "#a56cc1"]
    ]

    readonly property var dataLegendTypes: [
        [qsTr("Empty"), "#ffffff"],
        [qsTr("Data"), "#35a853"],
        [qsTr("Fragmented"), "#dc4f4a"],
        [qsTr("Defrag staging"), "#7c5ce0"],
        [qsTr("Not analyzed"), "#73777f"]
    ]

    readonly property var metadataLegendTypes: [
        [qsTr("FS headers"), "#8e62d9"],
        [qsTr("Journal"), "#e89b42"],
        [qsTr("Allocation table"), "#26a69a"],
        [qsTr("File metadata"), "#6c78d8"],
        [qsTr("Descriptors"), "#d4b33f"],
        [qsTr("Block bitmap"), "#38b9c7"],
        [qsTr("File bitmap"), "#d56da1"]
    ]

    readonly property var activityLegendTypes: [
        [qsTr("Pending read"), readActivityColor],
        [qsTr("Pending write"), writeActivityColor]
    ]

    function percent(value) {
        return (value / 100).toFixed(value > 0 && value < 100 ? 2 : 1) + "%"
    }

    function bytes(value) {
        const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"]
        let number = value
        let unit = 0
        while (number >= 1024 && unit < units.length - 1) {
            number /= 1024
            ++unit
        }
        return number.toFixed(unit === 0 ? 0 : 1) + " " + units[unit]
    }

    function integer(value) {
        return Math.floor(value).toLocaleString(Qt.locale(), "f", 0)
    }

    function field(index, fieldIndex) {
        return mapView.getUint16(index * recordBytes + 16 + fieldIndex * 2, true)
    }

    function uint64(index, byteOffset) {
        const offset = index * recordBytes + byteOffset
        return mapView.getUint32(offset, true)
            + mapView.getUint32(offset + 4, true) * 4294967296
    }

    function uint32(index, byteOffset) {
        return mapView.getUint32(index * recordBytes + byteOffset, true)
    }

    function pendingIoKind(index) {
        if (!pendingIoView || !mapView)
            return 0
        const binStart = uint64(index, 0)
        const binEnd = binStart + uint64(index, 8)
        let result = 0
        const count = Math.floor(pendingIoView.byteLength / 24)
        for (let i = 0; i < count; ++i) {
            const offset = i * 24
            const start = pendingIoView.getUint32(offset, true)
                + pendingIoView.getUint32(offset + 4, true) * 4294967296
            const length = pendingIoView.getUint32(offset + 8, true)
                + pendingIoView.getUint32(offset + 12, true) * 4294967296
            if (start < binEnd && start + length > binStart)
                result |= pendingIoView.getUint8(offset + 16)
        }
        return result
    }

    function cellCategory(index) {
        if (field(index, 4) > 0)
            return 2
        if (field(index, 2) > 0)
            return 1
        let winner = -1
        let winnerValue = 0
        for (let i = 0; i < metadataTypes.length; ++i) {
            const value = field(index, 5 + i)
            if (value > winnerValue) {
                winner = i
                winnerValue = value
            }
        }
        if (winner >= 0)
            return 3 + winner
        if (field(index, 3) > 0)
            return 12
        if (field(index, 1) > 0)
            return 13
        return 0
    }

    function categoryLabel(category) {
        if (category === 1)
            return qsTr("Fragmented data")
        if (category === 2)
            return qsTr("Defrag staging")
        if (category >= 3 && category <= 11)
            return metadataTypes[category - 3][1]
        if (category === 12)
            return qsTr("Not analyzed")
        if (category === 13)
            return qsTr("Contiguous data")
        return qsTr("Empty")
    }

    function categoryColor(category) {
        if (category === 1)
            return "#dc4f4a"
        if (category === 2)
            return "#7c5ce0"
        if (category >= 3 && category <= 11)
            return metadataTypes[category - 3][2]
        if (category === 12)
            return "#73777f"
        if (category === 13)
            return "#35a853"
        return "#ffffff"
    }

    function categoryCoverage(index, category) {
        if (category === 1)
            return field(index, 2)
        if (category === 2)
            return field(index, 4)
        if (category >= 3 && category <= 11)
            return field(index, category + 2)
        if (category === 12)
            return field(index, 3)
        if (category === 13)
            return field(index, 1)
        return 10000
    }

    function displayColor(index) {
        const category = cellCategory(index)
        const color = categoryColor(category)
        if (category === 0)
            return color
        // Keep a partially occupied category identifiable while making it
        // visibly lighter. Exact percentages remain available on hover.
        const occupied = Math.max(0, Math.min(1,
            categoryCoverage(index, category) / 10000))
        return Qt.lighter(color, 1 + (1 - occupied) * 0.35)
    }

    function grid(width, height) {
        const count = Math.max(1, binCount)
        const gap = 2
        const cell = 9
        const columns = Math.max(1, Math.floor((width + gap) / (cell + gap)))
        const rows = Math.ceil(count / columns)
        const gridWidth = columns * cell + (columns - 1) * gap
        const gridHeight = rows * cell + (rows - 1) * gap
        return {
            columns: columns,
            rows: rows,
            cell: cell,
            gap: gap,
            x: Math.floor((width - gridWidth) / 2),
            y: Math.floor((height - gridHeight) / 2)
        }
    }

    function requestRebuild(geometryChanged) {
        rebuildPending = true
        if (geometryChanged) {
            geometryPending = true
            resizeDebounce.restart()
            return
        }
        if (!workerBusy && !resizeDebounce.running)
            dispatchRebuild()
    }

    function dispatchRebuild() {
        if (!rebuildPending)
            return
        rebuildPending = false
        workerBusy = true
        ++rebuildGeneration
        rebuildRequested(
            canvas.width,
            canvas.height,
            capacityBytes,
            useAnalysis,
            rebuildGeneration
        )
    }

    function statistics(index) {
        if (!mapView || index < 0 || index >= binCount)
            return []
        const rows = []
        const append = function(label, value) {
            if ((value || 0) > 0)
                rows.push({ label: label, value: value })
        }
        append(qsTr("Fragmented data"), field(index, 2))
        append(qsTr("Contiguous data"), field(index, 1))
        append(qsTr("Not analyzed"), field(index, 3))
        append(qsTr("Defrag staging"), field(index, 4))
        for (let i = 0; i < metadataTypes.length; ++i) {
            const type = metadataTypes[i]
            append(type[1], field(index, 5 + i))
        }
        let described = field(index, 0) + field(index, 1)
            + field(index, 2) + field(index, 3) + field(index, 4)
        for (let i = 0; i < metadataTypes.length; ++i)
            described += field(index, 5 + i)
        append(qsTr("Empty"), Math.min(10000, field(index, 0) + Math.max(0, 10000 - described)))
        return rows
    }

    function relatedItems(index) {
        if (!mapView || !detailsProvider || index < 0 || index >= binCount)
            return []
        const items = []
        for (let slot = 0; slot < maxContributors; ++slot) {
            const offset = contributorOffset + slot * 6
            const fileIndex = uint32(index, offset)
            const coverage = field(index, (offset + 4 - 16) / 2)
            if (fileIndex !== 4294967295 && coverage > 0) {
                items.push({
                    label: String(detailsProvider.map_file_path(fileIndex)),
                    value: coverage
                })
            }
        }
        items.sort(function(left, right) {
            return right.value - left.value
        })
        return items.slice(0, 5)
    }

    onSourceRevisionChanged: {
        hoveredIndex = -1
        requestRebuild(false)
    }
    function refreshPendingIo() {
        if (!pendingIoData || pendingIoData.byteLength === 0) {
            pendingIoView = null
        } else {
            try {
                pendingIoView = new DataView(pendingIoData)
            } catch (error) {
                pendingIoView = null
            }
        }
        canvas.requestPaint()
    }
    onPendingIoDataChanged: refreshPendingIo()
    onPendingIoRevisionChanged: refreshPendingIo()
    onVolumeIdChanged: {
        // Selection is part of the map identity. Drop the previous volume's
        // pixels immediately, then publish a map for the new selection.
        hoveredIndex = -1
        mapView = null
        binCount = 0
        geometryPending = true
        requestRebuild(false)
    }
    onCapacityBytesChanged: {
        hoveredIndex = -1
        requestRebuild(true)
    }
    onUseAnalysisChanged: {
        hoveredIndex = -1
        geometryPending = true
        requestRebuild(false)
    }
    onRenderedGenerationChanged: {
        if (renderedGeneration !== rebuildGeneration)
            return
        workerBusy = false
        // A default/null QByteArray becomes `null` rather than an empty
        // ArrayBuffer in QML. There is simply no map to draw in that state.
        if (!mapData || mapData.byteLength === 0) {
            mapView = null
            binCount = 0
            canvas.requestPaint()
        } else {
            try {
                mapView = new DataView(mapData)
                binCount = Math.floor(mapView.byteLength / recordBytes)
                // A live map update normally preserves the tile count. Canvas
                // is imperative, so replacing its data buffer does not
                // schedule a repaint by itself.
                canvas.requestPaint()
            } catch (error) {
                console.warn("Could not read drive map buffer:", error)
                mapView = null
                binCount = 0
            }
        }
        // Publish every completed frame before starting the next one. During
        // a scan, updates can arrive faster than aggregation; skipping here
        // would otherwise starve the canvas until analysis finishes.
        if (rebuildPending) {
            if (!resizeDebounce.running)
                dispatchRebuild()
            return
        }
        geometryPending = false
    }
    onBinCountChanged: canvas.requestPaint()

    Component.onCompleted: requestRebuild(true)

    Timer {
        id: resizeDebounce
        interval: 80
        repeat: false
        onTriggered: {
            if (root.rebuildPending && !root.workerBusy)
                root.dispatchRebuild()
        }
    }

    Rectangle {
        anchors.fill: parent
        color: Kirigami.Theme.backgroundColor
        border.color: Kirigami.Theme.disabledTextColor
        border.width: 1
    }

    Canvas {
        id: canvas
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.bottom: legend.top
        anchors.margins: 6

        visible: !root.geometryPending
        onWidthChanged: root.requestRebuild(true)
        onHeightChanged: root.requestRebuild(true)
        onPaint: {
            const ctx = getContext("2d")
            ctx.reset()
            if (!root.mapView || root.binCount === 0)
                return
            const layout = root.grid(width, height)
            for (let i = 0; i < root.binCount; ++i) {
                const column = i % layout.columns
                const row = Math.floor(i / layout.columns)
                const x = layout.x + column * (layout.cell + layout.gap)
                const y = layout.y + row * (layout.cell + layout.gap)
                ctx.fillStyle = root.displayColor(i)
                ctx.fillRect(x, y, layout.cell, layout.cell)
                ctx.strokeStyle = i === root.hoveredIndex
                    ? Kirigami.Theme.highlightedTextColor
                    : Qt.rgba(0.12, 0.14, 0.16, 0.85)
                ctx.lineWidth = i === root.hoveredIndex ? 2 : 1
                ctx.strokeRect(x + 0.5, y + 0.5, layout.cell - 1, layout.cell - 1)
                const activity = root.pendingIoKind(i)
                if (activity === 3) {
                    // A display cell can aggregate distinct pending reads and
                    // writes. Use separate edges for the two truthful states;
                    // nested contours look like one marker changing color.
                    ctx.lineWidth = 2
                    ctx.strokeStyle = root.readActivityColor
                    ctx.beginPath()
                    ctx.moveTo(x + 0.5, y + layout.cell - 0.5)
                    ctx.lineTo(x + 0.5, y + 0.5)
                    ctx.lineTo(x + layout.cell - 0.5, y + 0.5)
                    ctx.stroke()
                    ctx.strokeStyle = root.writeActivityColor
                    ctx.beginPath()
                    ctx.moveTo(x + layout.cell - 0.5, y + 0.5)
                    ctx.lineTo(x + layout.cell - 0.5, y + layout.cell - 0.5)
                    ctx.lineTo(x + 0.5, y + layout.cell - 0.5)
                    ctx.stroke()
                } else if (activity & 1) {
                    ctx.strokeStyle = root.readActivityColor
                    ctx.lineWidth = 2
                    ctx.strokeRect(x + 0.5, y + 0.5, layout.cell - 1, layout.cell - 1)
                } else if (activity & 2) {
                    ctx.strokeStyle = root.writeActivityColor
                    ctx.lineWidth = 2
                    ctx.strokeRect(
                        x + 0.5,
                        y + 0.5,
                        layout.cell - 1,
                        layout.cell - 1
                    )
                }
            }
        }

        MouseArea {
            id: hoverArea
            anchors.fill: parent
            hoverEnabled: true
            onExited: {
                root.hoveredIndex = -1
                canvas.requestPaint()
            }
            onPositionChanged: function(mouse) {
                const layout = root.grid(width, height)
                const localX = mouse.x - layout.x
                const localY = mouse.y - layout.y
                const column = Math.floor(localX / (layout.cell + layout.gap))
                const row = Math.floor(localY / (layout.cell + layout.gap))
                const insideCell = localX >= 0 && localY >= 0
                    && localX % (layout.cell + layout.gap) < layout.cell
                    && localY % (layout.cell + layout.gap) < layout.cell
                const index = row * layout.columns + column
                root.hoveredIndex = insideCell && column >= 0 && row >= 0
                    && column < layout.columns && index < root.binCount ? index : -1
                canvas.requestPaint()
            }
            Controls.ToolTip {
                id: hoverTip
                readonly property var stats: root.hoveredIndex >= 0
                    ? root.statistics(root.hoveredIndex) : []
                readonly property var related: root.hoveredIndex >= 0
                    ? root.relatedItems(root.hoveredIndex) : []
                visible: hoverArea.containsMouse && root.hoveredIndex >= 0
                delay: Kirigami.Units.toolTipDelay
                timeout: -1
                width: Math.min(500, Math.max(360, hoverArea.width - 12))
                x: Math.max(0, Math.min(
                    hoverArea.width - width,
                    hoverArea.mouseX + 14
                ))
                y: Math.max(0, Math.min(
                    hoverArea.height - height,
                    hoverArea.mouseY + height + 18 <= hoverArea.height
                        ? hoverArea.mouseY + 18
                        : hoverArea.mouseY - height - 12
                ))

                contentItem: ColumnLayout {
                    spacing: Kirigami.Units.smallSpacing

                    Controls.Label {
                        Layout.fillWidth: true
                        text: root.hoveredIndex >= 0
                            ? root.categoryLabel(root.cellCategory(root.hoveredIndex)) : ""
                        font.bold: true
                    }
                    Controls.Label {
                        Layout.fillWidth: true
                        color: Kirigami.Theme.disabledTextColor
                        text: {
                            if (root.hoveredIndex < 0)
                                return ""
                            const start = root.uint64(root.hoveredIndex, 0)
                            const length = root.uint64(root.hoveredIndex, 8)
                            const firstSector = Math.floor(start / 512)
                            const lastSector = Math.max(firstSector,
                                Math.ceil((start + length) / 512) - 1)
                            return qsTr("Sectors %1 – %2")
                                .arg(root.integer(firstSector))
                                .arg(root.integer(lastSector))
                        }
                    }
                    Controls.Label {
                        Layout.fillWidth: true
                        text: root.hoveredIndex >= 0
                            ? qsTr("Block size: %1").arg(root.bytes(root.uint64(root.hoveredIndex, 8)))
                            : ""
                    }
                    Controls.Label {
                        Layout.fillWidth: true
                        visible: root.hoveredIndex >= 0
                            && root.pendingIoKind(root.hoveredIndex) !== 0
                        text: {
                            const activity = root.hoveredIndex >= 0
                                ? root.pendingIoKind(root.hoveredIndex) : 0
                            if (activity === 3)
                                return qsTr("Pending: read and write")
                            return activity === 1
                                ? qsTr("Pending: read") : qsTr("Pending: write")
                        }
                        font.bold: true
                    }

                    Repeater {
                        model: hoverTip.stats
                        delegate: RowLayout {
                            required property var modelData
                            Layout.fillWidth: true
                            spacing: Kirigami.Units.largeSpacing
                            Controls.Label {
                                Layout.fillWidth: true
                                text: modelData.label
                            }
                            Controls.Label {
                                text: root.percent(modelData.value)
                                horizontalAlignment: Text.AlignRight
                            }
                        }
                    }

                    Rectangle {
                        Layout.fillWidth: true
                        Layout.topMargin: Kirigami.Units.smallSpacing
                        Layout.bottomMargin: Kirigami.Units.smallSpacing
                        implicitHeight: 1
                        color: Kirigami.Theme.disabledTextColor
                        visible: hoverTip.related.length > 0
                    }
                    Controls.Label {
                        Layout.fillWidth: true
                        visible: hoverTip.related.length > 0
                        text: qsTr("Files in this block")
                        font.bold: true
                    }
                    Repeater {
                        model: hoverTip.related
                        delegate: RowLayout {
                            required property var modelData
                            Layout.fillWidth: true
                            spacing: Kirigami.Units.smallSpacing
                            Kirigami.Icon {
                                source: "document"
                                Layout.preferredWidth: 16
                                Layout.preferredHeight: 16
                            }
                            Controls.Label {
                                Layout.fillWidth: true
                                text: modelData.label
                                elide: Text.ElideMiddle
                            }
                            Controls.Label {
                                text: root.percent(modelData.value)
                                horizontalAlignment: Text.AlignRight
                            }
                        }
                    }
                }
            }
        }
    }

    Column {
        anchors.centerIn: canvas
        visible: root.geometryPending
        spacing: Kirigami.Units.smallSpacing

        Controls.BusyIndicator {
            anchors.horizontalCenter: parent.horizontalCenter
            running: parent.visible
        }
        Controls.Label {
            text: qsTr("Recomputing map…")
        }
    }

    Item {
        id: legend
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        anchors.leftMargin: Kirigami.Units.largeSpacing * 2
        anchors.rightMargin: Kirigami.Units.largeSpacing
        anchors.bottomMargin: Kirigami.Units.smallSpacing
        height: legendRow.implicitHeight + Kirigami.Units.smallSpacing * 2

        Component {
            id: legendEntry

            RowLayout {
                required property var modelData
                spacing: Kirigami.Units.smallSpacing

                Rectangle {
                    Layout.preferredWidth: 11
                    Layout.preferredHeight: 11
                    Layout.alignment: Qt.AlignVCenter
                    radius: 2
                    color: modelData[1]
                    border.color: "#34383d"
                    border.width: 1
                }
                Controls.Label {
                    Layout.alignment: Qt.AlignVCenter
                    text: modelData[0]
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                }
            }
        }

        Component {
            id: activityLegendEntry

            RowLayout {
                required property var modelData
                spacing: Kirigami.Units.smallSpacing

                Rectangle {
                    Layout.preferredWidth: 11
                    Layout.preferredHeight: 11
                    Layout.alignment: Qt.AlignVCenter
                    radius: 2
                    color: "transparent"
                    border.color: modelData[1]
                    border.width: 2
                }
                Controls.Label {
                    Layout.alignment: Qt.AlignVCenter
                    text: modelData[0]
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                }
            }
        }

        RowLayout {
            id: legendRow
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            spacing: Kirigami.Units.largeSpacing * 2

            RowLayout {
                Layout.alignment: Qt.AlignVCenter
                spacing: Kirigami.Units.largeSpacing * 2
                Repeater {
                    model: root.dataLegendTypes
                    delegate: legendEntry
                }
            }


            Rectangle {
                Layout.preferredWidth: 1
                Layout.preferredHeight: metadataLegend.implicitHeight
                Layout.alignment: Qt.AlignVCenter
                color: Kirigami.Theme.disabledTextColor
            }

            RowLayout {
                Layout.alignment: Qt.AlignVCenter
                spacing: Kirigami.Units.largeSpacing * 2
                Repeater {
                    model: root.activityLegendTypes
                    delegate: activityLegendEntry
                }
            }

            Rectangle {
                Layout.preferredWidth: 1
                Layout.preferredHeight: metadataLegend.implicitHeight
                Layout.alignment: Qt.AlignVCenter
                color: Kirigami.Theme.disabledTextColor
            }

            Flow {
                id: metadataLegend
                Layout.fillWidth: true
                Layout.alignment: Qt.AlignVCenter
                spacing: Kirigami.Units.largeSpacing
                Repeater {
                    model: root.metadataLegendTypes
                    delegate: legendEntry
                }
            }
        }
    }
}
