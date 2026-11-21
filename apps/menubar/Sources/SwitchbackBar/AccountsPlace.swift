import SwiftUI
import UsagePresentation

/// The account gauges at window size: the panel's own gauge view and rules, drawn wider.
struct AccountsPlace: View {
    let accounts: UsagePanelState
    var interactive = true
    var solid = false
    @Environment(\.displayScale) private var displayScale

    var body: some View {
        GeometryReader { geometry in
            let width = max(PanelMetrics.width, min(640, geometry.size.width - WindowMetrics.gutter * 2))
            DirectionPanelView(state: accounts, interactive: interactive, solidBackground: true, width: width, fillsHeight: true)
                .clipShape(RoundedRectangle(cornerRadius: WindowMetrics.cardRadius))
                .overlay(RoundedRectangle(cornerRadius: WindowMetrics.cardRadius)
                    .strokeBorder(Color.primary.opacity(0.08), lineWidth: 1 / displayScale))
                .frame(maxWidth: .infinity, alignment: .center)
                .padding(.bottom, WindowMetrics.gutter)
        }
    }
}
