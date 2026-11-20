import SwiftUI
import UsagePresentation

/// The visual looks (gauge, rings, strip) share one compact panel; the gauge is the default.
/// Common rules: big numbers, few labels, colour always paired with a symbol, reset as a short chip,
/// money and sentences only in details, and the seat to use now plus refresh one click away.
/// This file holds the rings and strip looks; the panel itself and the gauge are in PanelGauge.swift.
enum DirectionLayout {
    static let header = 48.0, hero = 70.0, footer = 30.0, sectionTitle = 20.0, sectionGap = 14.0, padding = 12.0
    static let ringTile = 118.0, ringGap = 8.0, stripHeat = 64.0, stripRow = 30.0

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
let modelTint = Color.purple

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

struct RingGrid: View {
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

// MARK: - Direction C: glance strip

/// When each Claude account's 5-hour window resets over the next 12 hours; later ones sit at the edge.
struct ResetHeatStrip: View {
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

struct GlanceStrip: View {
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

