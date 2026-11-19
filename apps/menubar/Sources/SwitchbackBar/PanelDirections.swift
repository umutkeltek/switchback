import SwiftUI
import UsagePresentation

/// Three visual explorations behind the hidden `panelStyle` flag (rings, gauge, strip).
/// Common rules: big numbers, few labels, colour always paired with a symbol, reset as a short chip,
/// money and sentences only in details, and the seat to use now plus refresh one click away.
enum DirectionLayout {
    static let header = 48.0, hero = 70.0, footer = 30.0, sectionTitle = 20.0, sectionGap = 14.0, padding = 12.0
    static let ringTile = 118.0, ringGap = 8.0, gaugeBlock = 78.0, stripHeat = 64.0, stripRow = 30.0

    static func height(_ data: DirectionPresentation, style: PanelStyle) -> Double {
        var content = padding * 2 + (data.best == nil ? 0 : hero + sectionGap)
        if style == .strip && data.sections.contains(where: { $0.id == "claude" }) { content += stripHeat + sectionGap }
        for (index, section) in data.sections.enumerated() {
            if index > 0 { content += sectionGap }
            content += sectionTitle
            let count = max(1, section.seats.count)
            switch style {
            case .rings:
                let rows = Double((count + 2) / 3)
                content += rows * ringTile + max(0, rows - 1) * ringGap
            case .gauge: content += gaugeBlock
            default: content += Double(count) * stripRow
            }
        }
        return header + content + footer
    }
}

extension SeatTone {
    var color: Color {
        switch self {
        case .good: .green
        case .low: .orange
        case .critical, .out: .red
        case .stale, .unknown: .secondary
        case .balance: .teal
        }
    }
}

/// Colour for a model's own limit (for example Fable): distinct from the green-amber-red room scale.
private let modelTint = Color.purple

struct DirectionPanelView: View {
    @Bindable var state: UsagePanelState
    var solidBackground = false
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency

    var body: some View {
        let data = Presenter.directions(state.presentation)
        VStack(spacing: 0) {
            header
            PanelRule()
            ScrollView {
                VStack(alignment: .leading, spacing: DirectionLayout.sectionGap) {
                    if let best = data.best { BestNowCard(best: best, picks: data.modelBest, style: state.style) }
                    if state.style == .strip, let claude = data.sections.first(where: { $0.id == "claude" }) {
                        ResetHeatStrip(seats: claude.seats)
                    }
                    ForEach(data.sections) { section in
                        VStack(alignment: .leading, spacing: 0) {
                            Text(section.title).font(.system(size: 11, weight: .semibold)).foregroundStyle(PanelColor.secondaryText)
                                .frame(height: DirectionLayout.sectionTitle, alignment: .topLeading)
                            switch state.style {
                            case .rings: RingGrid(seats: section.seats)
                            case .gauge: FuelGauge(seats: section.seats)
                            default: GlanceStrip(seats: section.seats)
                            }
                        }
                    }
                }
                .padding(DirectionLayout.padding)
            }
            .scrollIndicators(.automatic)
            PanelRule()
            HStack {
                Text(state.busy ? "Refreshing…" : state.checkedText).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                Spacer()
                Image(systemName: "slider.horizontal.3").font(.system(size: 12)).foregroundStyle(PanelColor.secondaryText)
                    .frame(width: 24, height: 24).accessibilityLabel("Options")
            }
            .padding(.horizontal, 12).frame(height: DirectionLayout.footer)
        }
        .frame(width: PanelMetrics.width, height: state.preferredHeight)
        .background {
            if reduceTransparency || solidBackground { PanelColor.canvas } else { Rectangle().fill(.regularMaterial) }
        }
    }
    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: "arrow.triangle.swap").font(.system(size: 13, weight: .semibold)).foregroundStyle(Color.accentColor)
                .accessibilityHidden(true)
            Text("Limits").font(.system(size: 13, weight: .semibold))
            Spacer()
            PanelIconButton(symbol: "magnifyingglass", label: "Find an account") {}
            PanelIconButton(symbol: "arrow.clockwise", label: "Refresh usage", action: state.onRefresh)
        }
        .padding(.horizontal, 12).frame(height: DirectionLayout.header)
    }
}

/// The one answer that matters: which seat to use now, with any model whose best seat differs.
private struct BestNowCard: View {
    let best: SeatGlyph
    let picks: [DirectionPresentation.ModelPick]
    let style: PanelStyle
    @Environment(\.colorSchemeContrast) private var contrast
    var body: some View {
        HStack(spacing: 12) {
            Group {
                switch style {
                case .rings: SeatRing(seat: best, size: 46, line: 5)
                case .gauge: VerticalFuel(seat: best).frame(width: 14, height: 44)
                default: Image(systemName: best.tone.symbol).font(.system(size: 26)).foregroundStyle(best.tone.color)
                }
            }
            .frame(width: 48, height: 48)
            VStack(alignment: .leading, spacing: 0) {
                Text("Use now").font(.system(size: 10, weight: .medium)).foregroundStyle(PanelColor.secondaryText)
                Text(best.name).font(.system(size: 15, weight: .semibold)).lineLimit(1)
                if let chip = best.chip {
                    Label(chip, systemImage: "arrow.clockwise").labelStyle(.titleAndIcon)
                        .font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText).monospacedDigit()
                }
            }
            Spacer(minLength: 4)
            if let pick = picks.first {
                VStack(alignment: .trailing, spacing: 1) {
                    Text(pick.model).font(.system(size: 10, weight: .semibold)).foregroundStyle(modelTint)
                    Text(pick.seat?.name ?? "—").font(.system(size: 12, weight: .semibold)).lineLimit(1)
                    Text(pick.roomText + "%").font(.system(size: 12, weight: .medium, design: .rounded)).monospacedDigit()
                        .foregroundStyle(PanelColor.secondaryText)
                }
                .padding(.trailing, 4)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel("For " + pick.model + ": " + (pick.seat?.name ?? "no fresh reading") + ", " + pick.roomText + " percent left")
            }
            HStack(alignment: .firstTextBaseline, spacing: 1) {
                Text(best.roomText).font(.system(size: 30, weight: .semibold, design: .rounded)).monospacedDigit()
                Text("%").font(.system(size: 13, weight: .semibold, design: .rounded)).foregroundStyle(PanelColor.secondaryText)
            }
        }
        .padding(.horizontal, 12).frame(height: DirectionLayout.hero)
        .background(Color.accentColor.opacity(contrast == .increased ? 0.18 : 0.10), in: RoundedRectangle(cornerRadius: 12))
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Use now: " + best.spoken)
    }
}

// MARK: - Direction A: rings

/// Weekly on the outer arc, 5-hour on the inner arc, a model's own limit as a tick on the outer track.
struct SeatRing: View {
    let seat: SeatGlyph
    var size = 56.0
    var line = 6.0
    @Environment(\.colorSchemeContrast) private var contrast
    var body: some View {
        let trusted = seat.room != nil || seat.tone == .out
        ZStack {
            arc(1, fraction: 1, color: Color.primary.opacity(contrast == .increased ? 0.22 : 0.09), dashed: !trusted)
            arc(0.68, fraction: 1, color: Color.primary.opacity(contrast == .increased ? 0.22 : 0.09), dashed: !trusted)
            if trusted {
                arc(1, fraction: seat.weekly ?? 0, color: Presenter.tone(room: seat.weekly).color)
                arc(0.68, fraction: seat.session ?? 0, color: Presenter.tone(room: seat.session).color)
                if let left = seat.modelLeft {
                    Capsule().fill(modelTint).frame(width: 3, height: line + 5)
                        .offset(y: -size / 2 + line / 2)
                        .rotationEffect(.degrees(360 * max(0, min(1, left))))
                }
            }
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
    private func arc(_ scale: Double, fraction: Double, color: Color, dashed: Bool = false) -> some View {
        Circle().trim(from: 0, to: max(0, min(1, fraction)))
            .stroke(color, style: StrokeStyle(lineWidth: line, lineCap: fraction >= 1 ? .butt : .round,
                                              dash: dashed ? [2, 3] : []))
            .rotationEffect(.degrees(-90))
            .padding(line / 2 + (1 - scale) * size / 2)
    }
}

private struct RingGrid: View {
    let seats: [SeatGlyph]
    var body: some View {
        LazyVGrid(columns: Array(repeating: GridItem(.flexible(), spacing: DirectionLayout.ringGap), count: 3),
                  spacing: DirectionLayout.ringGap) {
            ForEach(seats) { RingTile(seat: $0) }
        }
    }
}

private struct RingTile: View {
    let seat: SeatGlyph
    var body: some View {
        VStack(spacing: 5) {
            ZStack {
                if seat.tone == .balance {
                    Image(systemName: "creditcard").font(.system(size: 22)).foregroundStyle(seat.tone.color)
                } else {
                    SeatRing(seat: seat)
                    Text(seat.roomText).font(.system(size: 17, weight: .semibold, design: .rounded)).monospacedDigit()
                        .foregroundStyle(seat.room == nil && seat.tone != .out ? PanelColor.secondaryText : Color.primary)
                }
            }
            .frame(height: 58)
            HStack(spacing: 3) {
                Image(systemName: seat.tone.symbol).font(.system(size: 9, weight: .semibold)).foregroundStyle(seat.tone.color)
                Text(seat.name).font(.system(size: 11, weight: .semibold)).lineLimit(1)
            }
            Text(seat.tone == .balance ? (seat.chip ?? "—") : seat.chip ?? " ")
                .font(.system(size: seat.tone == .balance ? 12 : 10, weight: seat.tone == .balance ? .semibold : .regular))
                .monospacedDigit().foregroundStyle(seat.tone == .out ? Color.red : PanelColor.secondaryText).lineLimit(1)
        }
        .frame(maxWidth: .infinity).frame(height: DirectionLayout.ringTile)
        .background(PanelColor.card.opacity(0.6), in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(seat.best ? Color.accentColor : PanelColor.hairline,
                                                                  lineWidth: seat.best ? 1.5 : 1))
        .help(seat.spoken)
        .accessibilityElement(children: .ignore).accessibilityLabel(seat.spoken)
    }
}

// MARK: - Direction B: fuel gauge

private struct FuelGauge: View {
    let seats: [SeatGlyph]
    var body: some View {
        let models = seats.contains { $0.modelLeft != nil }
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 3) {
                ForEach(seats) { seat in
                    VStack(spacing: 2) {
                        GaugeSegment(fraction: seat.tone == .balance ? 1 : seat.room, tone: seat.tone, best: seat.best)
                            .frame(height: 24)
                        if models {
                            GaugeSegment(fraction: seat.modelLeft, tone: .balance, best: false, tint: modelTint).frame(height: 4)
                        }
                    }
                    .help(seat.spoken)
                }
            }
            HStack(alignment: .top, spacing: 3) {
                ForEach(seats) { seat in
                    VStack(spacing: 1) {
                        HStack(spacing: 2) {
                            Image(systemName: seat.tone.symbol).font(.system(size: 8, weight: .bold)).foregroundStyle(seat.tone.color)
                            Text(seat.name).font(.system(size: 10, weight: .medium)).lineLimit(1)
                        }
                        Text(seat.tone == .balance ? (seat.chip ?? "—") : seat.roomText)
                            .font(.system(size: 14, weight: .semibold, design: .rounded)).monospacedDigit()
                            .foregroundStyle(seat.room == nil && ![.out, .balance].contains(seat.tone) ? PanelColor.secondaryText : Color.primary)
                        Text(seat.tone == .balance ? " " : seat.chip ?? " ").font(.system(size: 9)).monospacedDigit()
                            .foregroundStyle(seat.tone == .out ? Color.red : PanelColor.secondaryText).lineLimit(1)
                    }
                    .frame(maxWidth: .infinity)
                    .accessibilityElement(children: .ignore).accessibilityLabel(seat.spoken)
                }
            }
        }
        .frame(height: DirectionLayout.gaugeBlock, alignment: .top)
    }
}

private struct GaugeSegment: View {
    let fraction: Double?
    let tone: SeatTone
    let best: Bool
    var tint: Color?
    var body: some View {
        GeometryReader { geometry in
            ZStack(alignment: .leading) {
                RoundedRectangle(cornerRadius: 4).fill(Color.primary.opacity(0.08))
                if let fraction {
                    RoundedRectangle(cornerRadius: 4).fill(tint ?? tone.color)
                        .frame(width: max(fraction > 0 ? 4 : 0, geometry.size.width * min(1, fraction)))
                } else {
                    RoundedRectangle(cornerRadius: 4).strokeBorder(Color.secondary.opacity(0.5),
                                                                    style: StrokeStyle(lineWidth: 1, dash: [2, 2]))
                }
            }
        }
        .overlay(RoundedRectangle(cornerRadius: 4).strokeBorder(best ? Color.accentColor : .clear, lineWidth: 1.5))
        .accessibilityHidden(true)
    }
}

/// The fuel gauge's hero: one vertical gauge for the seat to use now.
private struct VerticalFuel: View {
    let seat: SeatGlyph
    var body: some View {
        GeometryReader { geometry in
            ZStack(alignment: .bottom) {
                RoundedRectangle(cornerRadius: 4).fill(Color.primary.opacity(0.10))
                RoundedRectangle(cornerRadius: 4).fill(seat.tone.color)
                    .frame(height: geometry.size.height * min(1, seat.room ?? 0))
            }
        }
        .accessibilityHidden(true)
    }
}

// MARK: - Direction C: glance strip

/// When each Claude account's 5-hour window resets over the next 12 hours; later ones sit at the edge.
private struct ResetHeatStrip: View {
    let seats: [SeatGlyph]
    static let horizon = 12.0 * 3600
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text("Resets").font(.system(size: 11, weight: .semibold)).foregroundStyle(PanelColor.secondaryText)
                Spacer()
            }
            GeometryReader { geometry in
                let width = geometry.size.width
                ZStack(alignment: .topLeading) {
                    LinearGradient(colors: [Color.green.opacity(0.30), Color.orange.opacity(0.18), Color.primary.opacity(0.06)],
                                   startPoint: .leading, endPoint: .trailing)
                        .frame(height: 10).clipShape(Capsule()).offset(y: 14)
                    ForEach(Array(placed(width).enumerated()), id: \.element.seat.id) { _, mark in
                        let seat = mark.seat
                        VStack(spacing: 1) {
                            Text(String(seat.name.prefix(1))).font(.system(size: 9, weight: .bold))
                            Circle().fill(seat.tone.color).frame(width: 9, height: 9)
                                .overlay(Circle().strokeBorder(Color.primary.opacity(0.35), lineWidth: 0.5))
                        }
                        .position(x: mark.x, y: 12 - mark.lift)
                        .help(seat.name + " · 5-hour resets in " + duration(seat.sessionResetSeconds ?? 0))
                    }
                    HStack {
                        ForEach(["now", "3h", "6h", "9h", "12h"], id: \.self) { mark in
                            Text(mark).font(.system(size: 9)).foregroundStyle(.tertiary)
                            if mark != "12h" { Spacer() }
                        }
                    }
                    .offset(y: 28)
                }
            }
            .frame(height: 40)
        }
        .frame(height: DirectionLayout.stripHeat)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("5-hour resets: " + seats.compactMap { seat in
            seat.sessionResetSeconds.map { seat.name + " in " + duration($0) }
        }.joined(separator: ", "))
    }
    /// Markers closer than a label's width step up so initials never overlap.
    private func placed(_ width: Double) -> [(seat: SeatGlyph, x: Double, lift: Double)] {
        var result: [(seat: SeatGlyph, x: Double, lift: Double)] = []
        let sorted = seats.filter { $0.sessionResetSeconds != nil }.sorted { $0.sessionResetSeconds! < $1.sessionResetSeconds! }
        for seat in sorted {
            let x = max(6, min(width - 6, width * min(1, seat.sessionResetSeconds! / Self.horizon)))
            let lift = result.last.map { abs($0.x - x) < 12 ? $0.lift + 11 : 0 } ?? 0
            result.append((seat, x, min(lift, 22)))
        }
        return result
    }
    private func duration(_ seconds: Double) -> String {
        let minutes = Int(seconds / 60)
        return minutes >= 60 ? "\(minutes / 60)h \(minutes % 60)m" : "\(minutes)m"
    }
}

private struct GlanceStrip: View {
    let seats: [SeatGlyph]
    var body: some View {
        VStack(spacing: 0) {
            ForEach(seats) { seat in
                HStack(spacing: 8) {
                    Image(systemName: seat.tone.symbol).font(.system(size: 12)).foregroundStyle(seat.tone.color).frame(width: 16)
                    Text(seat.name).font(.system(size: 12, weight: seat.best ? .semibold : .medium)).lineLimit(1)
                        .foregroundStyle(seat.best ? Color.accentColor : Color.primary)
                        .frame(width: 74, alignment: .leading)
                    if seat.tone == .balance {
                        Spacer()
                        Text(seat.chip ?? "—").font(.system(size: 12, weight: .semibold)).monospacedDigit()
                    } else {
                        VStack(spacing: 2) {
                            StripBar(fraction: seat.session)
                            StripBar(fraction: seat.weekly, tick: seat.modelLeft)
                        }
                        Text(seat.chip ?? "").font(.system(size: 10)).monospacedDigit()
                            .foregroundStyle(seat.tone == .out ? Color.red : PanelColor.secondaryText)
                            .frame(width: 58, alignment: .trailing).lineLimit(1)
                    }
                }
                .frame(height: DirectionLayout.stripRow)
                .help(seat.spoken)
                .accessibilityElement(children: .ignore).accessibilityLabel(seat.spoken)
            }
        }
    }
}

/// A colour-only bar; the number is in the tooltip and the VoiceOver label.
private struct StripBar: View {
    let fraction: Double?
    var tick: Double?
    var body: some View {
        GeometryReader { geometry in
            ZStack(alignment: .leading) {
                Capsule().fill(Color.primary.opacity(0.08))
                if let fraction {
                    Capsule().fill(Presenter.tone(room: fraction).color)
                        .frame(width: max(fraction > 0 ? 5 : 0, geometry.size.width * min(1, fraction)))
                }
            }
            .overlay(alignment: .leading) {
                if let tick {
                    Rectangle().fill(modelTint).frame(width: 2, height: 9)
                        .offset(x: geometry.size.width * min(1, tick) - 1)
                }
            }
        }
        .frame(height: 5)
        .accessibilityHidden(true)
    }
}

// MARK: - Status item mocks

/// A crop of a menu bar with the direction's status item, for previews only.
struct StatusItemMock: View {
    let style: PanelStyle
    let data: DirectionPresentation
    let claude: [SeatGlyph]
    var body: some View {
        HStack(spacing: 14) {
            Image(systemName: "wifi")
            item
                .fixedSize()
                .padding(.horizontal, 6).frame(height: 22)
                .background(Color.primary.opacity(0.10), in: RoundedRectangle(cornerRadius: 5))
            Image(systemName: "magnifyingglass")
            Text("Thu 9:41").font(.system(size: 13, weight: .medium)).monospacedDigit()
        }
        .font(.system(size: 13, weight: .medium))
        .foregroundStyle(Color.primary)
        .padding(.horizontal, 12).frame(height: 28)
        .background(Color(nsColor: .windowBackgroundColor))
    }
    @ViewBuilder private var item: some View {
        let best = data.best
        switch style {
        case .rings:
            HStack(spacing: 4) {
                if let best { SeatRing(seat: best, size: 15, line: 2.2) }
                Text(best?.roomText ?? "—").font(.system(size: 12, weight: .semibold)).monospacedDigit()
            }
        case .gauge:
            HStack(spacing: 4) {
                HStack(alignment: .bottom, spacing: 1.5) {
                    ForEach(claude) { seat in
                        ZStack(alignment: .bottom) {
                            RoundedRectangle(cornerRadius: 1).fill(Color.primary.opacity(0.18)).frame(width: 3, height: 13)
                            RoundedRectangle(cornerRadius: 1).fill(seat.tone.color)
                                .frame(width: 3, height: 13 * max(0.08, min(1, seat.room ?? 0)))
                        }
                    }
                }
                Text(best?.roomText ?? "—").font(.system(size: 12, weight: .semibold)).monospacedDigit()
            }
        default:
            HStack(spacing: 4) {
                Image(systemName: best?.tone.symbol ?? "questionmark.circle").font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(best?.tone.color ?? .secondary)
                Text((best?.roomText ?? "—") + "%").font(.system(size: 12, weight: .semibold)).monospacedDigit()
            }
        }
    }
}
