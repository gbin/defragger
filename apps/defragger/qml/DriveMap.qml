import QtQuick
import QtQuick.Controls as Controls
import org.kde.kirigami as Kirigami

Item {
    id: root

    property string mapJson: "[]"
    property double capacityBytes: 0
    property bool useAnalysis: false
    property int sourceRevision: 0
    property int renderedGeneration: 0
    property var bins: []
    property int hoveredIndex: -1
    property bool workerBusy: false
    property bool rebuildPending: false
    property bool geometryPending: false
    property int rebuildGeneration: 0

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

    function metadataWinner(metadata) {
        let winner = null
        for (let i = 0; i < metadataTypes.length; ++i) {
            const type = metadataTypes[i]
            const value = metadata[type[0]] || 0
            if (value > 0 && (!winner || value > winner.value))
                winner = { value: value, label: type[1], color: type[2] }
        }
        return winner
    }

    // One cell, one color. Fragmentation has the highest priority, followed
    // by typed metadata, unknown allocation, normal data, and finally empty.
    function cellStyle(bin) {
        const mix = bin.mix
        if ((mix.fragmented_data || 0) > 0)
            return { label: qsTr("Fragmented data"), color: "#dc4f4a", coverage: mix.fragmented_data }
        const metadata = metadataWinner(mix.metadata || ({}))
        if (metadata)
            return { label: metadata.label, color: metadata.color, coverage: metadata.value }
        if ((mix.unscanned_data || 0) > 0)
            return { label: qsTr("Not analyzed"), color: "#73777f", coverage: mix.unscanned_data }
        if ((mix.contiguous_data || 0) > 0)
            return { label: qsTr("Contiguous data"), color: "#35a853", coverage: mix.contiguous_data }
        return { label: qsTr("Empty"), color: "#ffffff", coverage: 10000 }
    }

    function displayColor(style) {
        if (style.label === qsTr("Empty"))
            return style.color
        // Keep a partially occupied category identifiable while making it
        // visibly lighter. Exact percentages remain available on hover.
        const occupied = Math.max(0, Math.min(1, style.coverage / 10000))
        return Qt.lighter(style.color, 1 + (1 - occupied) * 0.35)
    }

    function grid(width, height) {
        const count = Math.max(1, bins.length)
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

    function detailText(bin) {
        if (!bin)
            return ""
        const style = cellStyle(bin)
        const start = bin.offset_bytes
        const end = start + bin.length_bytes
        const lines = [
            style.label + qsTr(" (display priority)"),
            bytes(start) + " – " + bytes(end),
            qsTr("Cell span: %1").arg(bytes(bin.length_bytes))
        ]
        const mix = bin.mix
        const append = function(label, value) {
            if ((value || 0) > 0)
                lines.push(label + ": " + percent(value))
        }
        append(qsTr("Fragmented data"), mix.fragmented_data)
        append(qsTr("Contiguous data"), mix.contiguous_data)
        append(qsTr("Not analyzed"), mix.unscanned_data)
        for (let i = 0; i < metadataTypes.length; ++i) {
            const type = metadataTypes[i]
            append(type[1], (mix.metadata || ({}))[type[0]])
        }
        let described = (mix.free || 0) + (mix.fragmented_data || 0)
            + (mix.contiguous_data || 0) + (mix.unscanned_data || 0)
        for (let i = 0; i < metadataTypes.length; ++i)
            described += (mix.metadata || ({}))[metadataTypes[i][0]] || 0
        append(qsTr("Empty"), Math.min(10000, (mix.free || 0) + Math.max(0, 10000 - described)))
        return lines.join("\n")
    }

    onSourceRevisionChanged: {
        hoveredIndex = -1
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
        if (rebuildPending) {
            if (!resizeDebounce.running)
                dispatchRebuild()
            return
        }
        try {
            bins = JSON.parse(mapJson)
        } catch (_) {
            bins = []
        }
        geometryPending = false
    }
    onBinsChanged: canvas.requestPaint()

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
            if (root.bins.length === 0)
                return
            const layout = root.grid(width, height)
            for (let i = 0; i < root.bins.length; ++i) {
                const column = i % layout.columns
                const row = Math.floor(i / layout.columns)
                const x = layout.x + column * (layout.cell + layout.gap)
                const y = layout.y + row * (layout.cell + layout.gap)
                const style = root.cellStyle(root.bins[i])
                ctx.fillStyle = root.displayColor(style)
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
                    && column < layout.columns && index < root.bins.length ? index : -1
                canvas.requestPaint()
            }
            Controls.ToolTip.visible: containsMouse && root.hoveredIndex >= 0
            Controls.ToolTip.delay: Kirigami.Units.toolTipDelay
            Controls.ToolTip.text: root.hoveredIndex >= 0
                ? root.detailText(root.bins[root.hoveredIndex]) : ""
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
