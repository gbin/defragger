import QtQuick
import QtQuick.Controls as Controls
import org.kde.kirigami as Kirigami

Item {
    id: root

    property var mapData
    property string volumeId: ""
    property double capacityBytes: 0
    property bool useAnalysis: false
    property int sourceRevision: 0
    property int renderedGeneration: 0
    property var mapView: null
    property int binCount: 0
    property int hoveredIndex: -1
    property bool workerBusy: false
    property bool rebuildPending: false
    property bool geometryPending: false
    property int rebuildGeneration: 0
    readonly property int recordBytes: 42

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

    readonly property var legendTypes: [
        [qsTr("Empty"), "#ffffff"],
        [qsTr("Data"), "#35a853"],
        [qsTr("Fragmented"), "#dc4f4a"],
        [qsTr("Not analyzed"), "#73777f"],
        [qsTr("FS headers"), "#8e62d9"],
        [qsTr("Journal"), "#e89b42"],
        [qsTr("Allocation table"), "#26a69a"],
        [qsTr("File metadata"), "#6c78d8"],
        [qsTr("Descriptors"), "#d4b33f"],
        [qsTr("Block bitmap"), "#38b9c7"],
        [qsTr("File bitmap"), "#d56da1"]
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

    function field(index, fieldIndex) {
        return mapView.getUint16(index * recordBytes + 16 + fieldIndex * 2, true)
    }

    function uint64(index, byteOffset) {
        const offset = index * recordBytes + byteOffset
        return mapView.getUint32(offset, true)
            + mapView.getUint32(offset + 4, true) * 4294967296
    }

    function cellCategory(index) {
        if (field(index, 2) > 0)
            return 1
        let winner = -1
        let winnerValue = 0
        for (let i = 0; i < metadataTypes.length; ++i) {
            const value = field(index, 4 + i)
            if (value > winnerValue) {
                winner = i
                winnerValue = value
            }
        }
        if (winner >= 0)
            return 2 + winner
        if (field(index, 3) > 0)
            return 11
        if (field(index, 1) > 0)
            return 12
        return 0
    }

    function categoryLabel(category) {
        if (category === 1)
            return qsTr("Fragmented data")
        if (category >= 2 && category <= 10)
            return metadataTypes[category - 2][1]
        if (category === 11)
            return qsTr("Not analyzed")
        if (category === 12)
            return qsTr("Contiguous data")
        return qsTr("Empty")
    }

    function categoryColor(category) {
        if (category === 1)
            return "#dc4f4a"
        if (category >= 2 && category <= 10)
            return metadataTypes[category - 2][2]
        if (category === 11)
            return "#73777f"
        if (category === 12)
            return "#35a853"
        return "#ffffff"
    }

    function categoryCoverage(index, category) {
        if (category === 1)
            return field(index, 2)
        if (category >= 2 && category <= 10)
            return field(index, category + 2)
        if (category === 11)
            return field(index, 3)
        if (category === 12)
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

    function detailText(index) {
        if (!mapView || index < 0 || index >= binCount)
            return ""
        const category = cellCategory(index)
        const start = uint64(index, 0)
        const length = uint64(index, 8)
        const end = start + length
        const lines = [
            categoryLabel(category) + qsTr(" (display priority)"),
            bytes(start) + " – " + bytes(end),
            qsTr("Cell span: %1").arg(bytes(length))
        ]
        const append = function(label, value) {
            if ((value || 0) > 0)
                lines.push(label + ": " + percent(value))
        }
        append(qsTr("Fragmented data"), field(index, 2))
        append(qsTr("Contiguous data"), field(index, 1))
        append(qsTr("Not analyzed"), field(index, 3))
        for (let i = 0; i < metadataTypes.length; ++i) {
            const type = metadataTypes[i]
            append(type[1], field(index, 4 + i))
        }
        let described = field(index, 0) + field(index, 1)
            + field(index, 2) + field(index, 3)
        for (let i = 0; i < metadataTypes.length; ++i)
            described += field(index, 4 + i)
        append(qsTr("Empty"), Math.min(10000, field(index, 0) + Math.max(0, 10000 - described)))
        return lines.join("\n")
    }

    onSourceRevisionChanged: {
        hoveredIndex = -1
        requestRebuild(false)
    }
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
                visible: hoverArea.containsMouse && root.hoveredIndex >= 0
                delay: Kirigami.Units.toolTipDelay
                timeout: -1
                text: root.hoveredIndex >= 0
                    ? root.detailText(root.hoveredIndex) : ""
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

    Flow {
        id: legend
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        anchors.margins: Kirigami.Units.smallSpacing
        height: 43
        spacing: Kirigami.Units.smallSpacing
        Repeater {
            model: root.legendTypes
            delegate: Row {
                required property var modelData
                spacing: 3
                Rectangle {
                    width: 10
                    height: 10
                    radius: 2
                    color: modelData[1]
                    border.color: "#34383d"
                    border.width: 1
                }
                Controls.Label {
                    text: modelData[0]
                    font.pixelSize: Kirigami.Theme.smallFont.pixelSize
                }
            }
        }
    }
}
