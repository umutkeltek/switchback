import AppKit
import SwiftUI
import UsagePresentation

/// The gauge look's menu bar item: one mini bar per pinned Claude seat, then the best seat's number.
/// Drawn when the item is displayed, so its colours follow the menu bar's appearance.
enum StatusGaugeImage {
    struct Bar { let fraction: Double?; let tone: SeatTone }

    static func bars(_ seats: [SeatGlyph]) -> [Bar] {
        seats.map { Bar(fraction: $0.tone == .out ? 0 : $0.room, tone: $0.tone) }
    }
    static func numberWidth(_ number: String, font: NSFont) -> Double {
        (number as NSString).size(withAttributes: [.font: font]).width.rounded(.up)
    }
    /// nil when the bars and number do not fit the menu bar budget: the item then shows the icon.
    static func make(bars: [Bar], number: String, font: NSFont, budget: Double = StatusGauge.budget) -> NSImage? {
        let textWidth = numberWidth(number, font: font)
        guard StatusGauge.fits(bars: bars.count, numberWidth: textWidth, budget: budget) else { return nil }
        let width = StatusGauge.width(bars: bars.count, numberWidth: textWidth)
        let height = 18.0
        let image = NSImage(size: NSSize(width: width, height: height), flipped: false) { _ in
            let top = (height + StatusGauge.barHeight) / 2
            for (index, bar) in bars.enumerated() {
                let x = Double(index) * (StatusGauge.barWidth + StatusGauge.barGap)
                let track = NSRect(x: x, y: top - StatusGauge.barHeight, width: StatusGauge.barWidth, height: StatusGauge.barHeight)
                NSColor.labelColor.withAlphaComponent(0.22).setFill()
                NSBezierPath(roundedRect: track, xRadius: 1, yRadius: 1).fill()
                if let fraction = bar.fraction {
                    // A sliver stays visible above zero; an out seat shows its red track instead.
                    let filled = fraction > 0 ? max(1.5, StatusGauge.barHeight * min(1, fraction)) : 0
                    let rect = NSRect(x: x, y: track.minY, width: StatusGauge.barWidth, height: filled)
                    color(bar.tone).setFill()
                    NSBezierPath(roundedRect: rect, xRadius: 1, yRadius: 1).fill()
                }
                if bar.tone == .out {
                    NSColor.systemRed.withAlphaComponent(0.55).setFill()
                    NSBezierPath(roundedRect: track, xRadius: 1, yRadius: 1).fill()
                }
            }
            let attributes: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: NSColor.labelColor]
            let size = (number as NSString).size(withAttributes: attributes)
            (number as NSString).draw(at: NSPoint(x: width - textWidth, y: (height - size.height) / 2), withAttributes: attributes)
            return true
        }
        image.isTemplate = false
        return image
    }
    static func color(_ tone: SeatTone) -> NSColor {
        switch tone {
        case .good: .systemGreen
        case .low: .systemOrange
        case .critical, .out: .systemRed
        case .balance: .systemTeal
        case .stale, .unknown: .secondaryLabelColor
        }
    }
}

/// A crop of a menu bar with a look's status item, for previews only.
struct StatusItemMock: View {
    let style: PanelStyle
    let data: DirectionPresentation
    let claude: [SeatGlyph]
    /// The gauge item exactly as the app draws it; nil falls back to the icon.
    let gaugeImage: NSImage?
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
            if let gaugeImage {
                Image(nsImage: gaugeImage)
            } else {
                Image(systemName: "arrow.triangle.swap").font(.system(size: 13, weight: .medium))
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
