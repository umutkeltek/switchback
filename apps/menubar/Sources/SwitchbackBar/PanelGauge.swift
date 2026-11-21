import SwiftUI
import AppKit
import UsagePresentation

/// The compact panel shared by the visual looks. The gauge is the default look: one segmented gauge per
/// provider, one segment per account, the seat to use now on top, and a seat's details in place.
struct DirectionPanelView: View {
    @Bindable var state: UsagePanelState
    var interactive = true
    var solidBackground = false
    @FocusState private var searchFocused: Bool
    @FocusState private var focusedSeat: String?
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.displayScale) private var displayScale

    var body: some View {
        let data = state.directionData
        VStack(spacing: 0) {
            header
            if state.searchOpen { searchBar.padding(.horizontal, 12).frame(height: GaugeLayout.searchBar, alignment: .top) }
            PanelRule()
            ScrollViewReader { scroller in
                ScrollView {
                    VStack(alignment: .leading, spacing: GaugeLayout.sectionGap) {
                        if let best = data.best {
                            BestNowCard(best: best, picks: data.modelBest, style: state.style) { toggle(best.id) }
                        } else if let noRoom = data.noRoom {
                            NoRoomCard(notice: noRoom) { if let seat = noRoom.seat { toggle(seat.id) } }
                        } else if let noFresh = data.noFresh {
                            // The card keeps its place and size; clicking it refreshes.
                            NoFreshCard(notice: noFresh, busy: state.busy, action: state.onRefresh)
                        }
                        if state.style == .strip, let claude = data.sections.first(where: { $0.id == "claude" }) {
                            ResetHeatStrip(seats: claude.seats)
                        }
                        ForEach(data.sections) { section in
                            VStack(alignment: .leading, spacing: 0) {
                                Text(section.title).font(.system(size: 11, weight: .semibold)).foregroundStyle(PanelColor.secondaryText)
                                    .frame(height: GaugeLayout.sectionTitle, alignment: .topLeading)
                                    .accessibilityAddTraits(.isHeader)
                                switch state.style {
                                case .rings: RingGrid(seats: section.seats)
                                case .strip: GlanceStrip(seats: section.seats)
                                default: gauge(section, all: data.seats)
                                }
                            }
                            if state.style == .gauge, let detail = state.openDetail,
                               section.seats.contains(where: { $0.id == detail.key }) {
                                SeatDetailCard(detail: detail, onPin: state.onPin) { close() }
                                    .id("seat-detail")
                                    .padding(.top, GaugeLayout.rowGap - GaugeLayout.sectionGap)
                                    .transition(.opacity)
                            }
                        }
                        if let line = data.noReading { NoReadingLineView(line: line) }
                        if data.sections.isEmpty && data.noReading == nil { emptyState }
                    }
                    .padding(GaugeLayout.padding)
                }
                .scrollIndicators(.automatic)
                .onChange(of: state.openSeat) { _, seat in
                    guard seat != nil else { return }
                    withAnimation(PanelMotion.disclosure(reduceMotion: reduceMotion, pointerInitiated: true)) {
                        scroller.scrollTo("seat-detail", anchor: .bottom)
                    }
                }
            }
            PanelRule()
            footer
        }
        .frame(width: PanelMetrics.width, height: state.preferredHeight)
        .background {
            if reduceTransparency || solidBackground { PanelColor.canvas } else { Rectangle().fill(.regularMaterial) }
        }
        .onExitCommand {
            if state.openSeat != nil { close() }
            else if state.searchOpen { closeSearch() }
            else { state.onClose() }
        }
        .onChange(of: state.preferredHeight) { _, height in state.onResize(height) }
        .onChange(of: state.selectedProvider) { _, provider in
            if interactive { state.onProviderChange(provider) }
        }
        .onChange(of: state.focusSearchRequest) { _, _ in
            // Every opening starts closed: no details, no search.
            state.openSeat = nil
            state.searchOpen = false
        }
    }

    // MARK: Header, search, footer

    private var header: some View {
        HStack(spacing: 6) {
            Image(systemName: "arrow.triangle.swap").font(.system(size: 13, weight: .semibold)).foregroundStyle(Color.accentColor)
                .accessibilityHidden(true)
            Text("Limits").font(.system(size: 13, weight: .semibold))
            if let filter = state.filterTitle {
                Text("· " + filter).font(.system(size: 12)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
            }
            Spacer()
            filterMenu
            PanelIconButton(symbol: "magnifyingglass", label: state.searchOpen ? "Close search" : "Find an account",
                            selected: state.searchOpen) {
                if state.searchOpen { closeSearch() } else { state.searchOpen = true; searchFocused = true }
            }
            .keyboardShortcut("f", modifiers: .command)
            if state.busy {
                ProgressView().controlSize(.small).frame(width: 24, height: 24).help("Refreshing account usage")
            } else {
                PanelIconButton(symbol: "arrow.clockwise", label: "Refresh usage", action: state.onRefresh)
                    .keyboardShortcut("r", modifiers: .command)
            }
        }
        .padding(.horizontal, 12).frame(height: GaugeLayout.header)
    }
    private var filterMenu: some View {
        let active = state.selectedProvider != "all" || state.pinnedOnly
        return Menu {
            Picker("Provider", selection: $state.selectedProvider) {
                Text("All providers").tag("all")
                ForEach(state.presentation.providers, id: \.key) { provider in
                    Text(Presenter.displayTitle(key: provider.key, title: provider.title)).tag(provider.key)
                }
            }
            .pickerStyle(.inline)
            Divider()
            Toggle("Pinned accounts only", isOn: $state.pinnedOnly)
        } label: {
            Image(systemName: active ? "line.3.horizontal.decrease.circle.fill" : "line.3.horizontal.decrease.circle")
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(active ? Color.accentColor : Color.secondary)
                .frame(width: 24, height: 24)
        }
        .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
        .help("Filter providers").accessibilityLabel(active ? "Filter providers, filter on" : "Filter providers").focusable()
    }
    private var searchBar: some View {
        HStack(spacing: 7) {
            Image(systemName: "magnifyingglass").font(.system(size: 12)).foregroundStyle(.tertiary).accessibilityHidden(true)
            TextField(text: $state.query, prompt: Text("Find an account").foregroundStyle(PanelColor.secondaryText)) { Text("Find an account") }
                .textFieldStyle(.plain).font(.system(size: 12)).focused($searchFocused)
                .accessibilityLabel("Find an account by label or email")
            if !state.query.isEmpty {
                Button { state.query = "" } label: {
                    Image(systemName: "xmark.circle.fill").foregroundStyle(.tertiary)
                }.buttonStyle(PanelPressStyle()).focusable().help("Clear search").accessibilityLabel("Clear search")
            }
        }
        .padding(.horizontal, 9).frame(height: 30)
        .background(Color.primary.opacity(0.04), in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(PanelColor.hairline, lineWidth: 1 / displayScale))
    }
    private var footer: some View {
        HStack(spacing: 5) {
            if state.failure != nil {
                // A failed refresh hides nothing: the numbers above are judged by their own sample times.
                Image(systemName: "exclamationmark.circle").font(.system(size: 10)).foregroundStyle(Color.orange)
                    .accessibilityHidden(true)
                Text(Presenter.failureFooter(readingsAgeSeconds: state.presentation.readingsAgeSeconds,
                                             hasReadings: state.presentation.accountCount > 0, busy: state.busy))
                    .font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText).lineLimit(1).truncationMode(.middle)
                    .help(state.failure ?? "")
            } else {
                // A check that ran is not a reading that arrived: old readings say their age, with an orange dot.
                let line = Presenter.footer(checked: state.checkedText,
                                            readingsAgeSeconds: state.presentation.readingsAgeSeconds, busy: state.busy)
                Circle().fill(state.busy || line.old ? Color.orange : Color.secondary.opacity(0.45)).frame(width: 5, height: 5)
                    .accessibilityHidden(true)
                Text(line.text)
                    .font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
            }
            Spacer()
            PanelOptionsMenu(state: state)
        }
        .padding(.horizontal, 12).frame(height: GaugeLayout.footer)
    }
    private var emptyState: some View {
        VStack(spacing: 8) {
            Image(systemName: "magnifyingglass").font(.system(size: 22)).foregroundStyle(.tertiary).accessibilityHidden(true)
            Text(state.busy && state.presentation.providers.isEmpty ? "Reading your accounts…"
                 : state.presentation.providers.isEmpty ? "No account readings yet" : "No matching accounts")
                .font(.system(size: 13, weight: .medium))
            if !state.presentation.providers.isEmpty {
                Button("Show all accounts") {
                    state.query = ""; state.selectedProvider = "all"; state.pinnedOnly = false
                }.buttonStyle(.link).font(.system(size: 11))
            }
        }
        .frame(maxWidth: .infinity).frame(height: GaugeLayout.emptyState)
    }

    // MARK: Gauge

    @ViewBuilder private func gauge(_ section: DirectionSection, all: [SeatGlyph]) -> some View {
        let columns = GaugeLayout.columns(section.seats.count)
        VStack(alignment: .leading, spacing: GaugeLayout.rowGap) {
            ForEach(Array(GaugeLayout.rows(section.seats).enumerated()), id: \.offset) { _, row in
                HStack(spacing: GaugeLayout.segmentGap) {
                    ForEach(row) { seat in
                        GaugeSegmentButton(seat: seat, modelBar: section.hasModelBar, selected: state.openSeat == seat.id,
                                           focused: focusedSeat == seat.id, openable: state.canOpen(seat.id)) { toggle(seat.id) }
                            .frame(maxWidth: .infinity)
                            .focused($focusedSeat, equals: seat.id)
                            .onKeyPress(.return) { toggle(seat.id); return .handled }
                            .onKeyPress(.leftArrow) { move(from: seat.id, by: -1, in: all); return .handled }
                            .onKeyPress(.rightArrow) { move(from: seat.id, by: 1, in: all); return .handled }
                            .onKeyPress(.upArrow) { move(from: seat.id, by: -columns, in: all); return .handled }
                            .onKeyPress(.downArrow) { move(from: seat.id, by: columns, in: all); return .handled }
                    }
                    // Empty columns keep every segment in the group the same width.
                    ForEach(row.count..<columns, id: \.self) { _ in Color.clear.frame(maxWidth: .infinity, maxHeight: 1) }
                }
            }
            if let legend = section.legend { GaugeLegend(model: legend, spoken: section.legendSpoken ?? legend) }
        }
    }
    private func move(from id: String, by step: Int, in seats: [SeatGlyph]) {
        guard let index = seats.firstIndex(where: { $0.id == id }) else { return }
        focusedSeat = seats[max(0, min(seats.count - 1, index + step))].id
    }
    private func toggle(_ id: String) {
        guard state.canOpen(id) else { return }
        withAnimation(PanelMotion.disclosure(reduceMotion: reduceMotion, pointerInitiated: PanelMotion.pointerInitiated)) {
            state.openSeat = state.openSeat == id ? nil : id
        }
    }
    private func close() {
        let seat = state.openSeat
        withAnimation(PanelMotion.disclosure(reduceMotion: reduceMotion, pointerInitiated: PanelMotion.pointerInitiated)) {
            state.openSeat = nil
        }
        if let seat { focusedSeat = seat }
    }
    private func closeSearch() {
        state.query = ""
        state.searchOpen = false
        searchFocused = false
    }
}

/// The one answer that matters: which seat to use now, with any model whose best seat differs on the right.
private struct BestNowCard: View {
    let best: SeatGlyph
    let picks: [DirectionPresentation.ModelPick]
    let style: PanelStyle
    let action: () -> Void
    @Environment(\.colorSchemeContrast) private var contrast
    var body: some View {
        Button(action: action) {
            HStack(spacing: 10) {
                Group {
                    switch style {
                    case .rings: SeatRing(seat: best, size: 46, line: 5)
                    case .strip: Image(systemName: best.tone.symbol).font(.system(size: 26)).foregroundStyle(best.tone.color)
                    default: VerticalFuel(seat: best).frame(width: 14, height: 44)
                    }
                }
                .frame(width: style == .rings ? 48 : 30, height: 48)
                VStack(alignment: .leading, spacing: 1) {
                    Text("Use now").font(.system(size: 10, weight: .medium)).foregroundStyle(PanelColor.secondaryText)
                    // A long name gives way in the middle; the number never does.
                    Text(best.name).font(.system(size: 15, weight: .semibold)).lineLimit(1).truncationMode(.middle)
                    HStack(spacing: 4) {
                        if let note = best.partialNote { Text(note).foregroundStyle(PanelColor.secondaryText).fixedSize() }
                        if let chip = best.chip { Text((best.partialNote == nil ? "" : "· ") + chip) }
                        if let age = best.age { Text("· " + age).foregroundStyle(.tertiary) }
                    }
                    .font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText).monospacedDigit().lineLimit(1)
                }
                .layoutPriority(1)
                Spacer(minLength: 4)
                if let pick = picks.first {
                    VStack(alignment: .trailing, spacing: 1) {
                        Text("For " + pick.model).font(.system(size: 10, weight: .semibold)).foregroundStyle(modelTint)
                        if pick.sameAsBest {
                            // The same seat as on the left: its name is said once, there.
                            Text(pick.roomText + "%").font(.system(size: 15, weight: .semibold, design: .rounded))
                                .monospacedDigit()
                        } else if let back = pick.backText {
                            // No seat has room for this model: say so, with when the first one is back.
                            Text("No room").font(.system(size: 12, weight: .semibold)).lineLimit(1)
                            Group {
                                if let seat = pick.backSeat, let backIn = pick.backIn {
                                    // The seat name gives way in the middle; "back in 1d 5h" stays whole.
                                    HStack(spacing: 3) {
                                        Text(seat).lineLimit(1).truncationMode(.middle)
                                        Text("back in " + backIn).fixedSize()
                                    }
                                } else {
                                    Text(back).lineLimit(1)
                                }
                            }
                            .font(.system(size: 11, weight: .medium)).monospacedDigit().foregroundStyle(Color.red)
                        } else {
                            Text(pick.seat?.name ?? "—").font(.system(size: 12, weight: .semibold)).lineLimit(1)
                                .truncationMode(.middle)
                            Text(pick.roomText + "%").font(.system(size: 12, weight: .medium, design: .rounded)).monospacedDigit()
                                .foregroundStyle(PanelColor.secondaryText)
                        }
                    }
                    .padding(.trailing, 4)
                    // Only a number: as narrow as it is, so the seat's name on the left keeps the room.
                    .frame(minWidth: pick.sameAsBest ? nil : 76, maxWidth: pick.sameAsBest ? nil : 116, alignment: .trailing)
                    .fixedSize(horizontal: pick.sameAsBest, vertical: false)
                }
                HStack(alignment: .firstTextBaseline, spacing: 1) {
                    Text(best.roomText).font(.system(size: 30, weight: .semibold, design: .rounded)).monospacedDigit()
                    Text("%").font(.system(size: 13, weight: .semibold, design: .rounded)).foregroundStyle(PanelColor.secondaryText)
                }
                .fixedSize()
            }
            .padding(.horizontal, 12).frame(height: GaugeLayout.hero)
            .background(Color.accentColor.opacity(contrast == .increased ? 0.18 : 0.10), in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(contrast == .increased ? Color.accentColor : .clear, lineWidth: 1))
            .contentShape(RoundedRectangle(cornerRadius: 12))
        }
        .buttonStyle(PanelPressStyle()).focusable()
        // Names cut in the middle on the card are whole here.
        .help(best.detail + picks.map { pick in
            "\nFor " + pick.model + ": " + (pick.backText.map { "no room · " + $0 }
                ?? (pick.seat?.detail.components(separatedBy: "\n").first ?? "no fresh reading"))
        }.joined())
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Use now: " + best.spoken + picks.map { pick in
            ". For " + pick.model + ": " + (pick.backText.map { "no seat has room, " + $0 }
                ?? (pick.seat?.name ?? "no fresh reading") + ", " + pick.roomText + " percent left")
        }.joined())
        .accessibilityHint("Shows this seat's details")
        .accessibilityAddTraits(.isButton)
    }
}

/// The gauge's hero: one vertical gauge for the seat to use now.
/// Every seat with a reading is at its limit: no seat is recommended. The card says so plainly and
/// names the seat that is back first; clicking it opens that seat's details.
private struct NoRoomCard: View {
    let notice: DirectionPresentation.NoRoom
    let action: () -> Void
    @Environment(\.colorSchemeContrast) private var contrast
    var body: some View {
        Button(action: action) {
            HStack(spacing: 12) {
                Image(systemName: SeatTone.out.symbol).font(.system(size: 26)).foregroundStyle(SeatTone.out.color)
                    .frame(width: 48, height: 48)
                VStack(alignment: .leading, spacing: 1) {
                    Text("Use now").font(.system(size: 10, weight: .medium)).foregroundStyle(PanelColor.secondaryText)
                    Text(notice.title).font(.system(size: 15, weight: .semibold)).lineLimit(1)
                    Text(notice.detail).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                        .monospacedDigit().lineLimit(1)
                }
                Spacer(minLength: 4)
                if let backIn = notice.backIn {
                    VStack(alignment: .trailing, spacing: 1) {
                        Text("Back in").font(.system(size: 10, weight: .medium)).foregroundStyle(PanelColor.secondaryText)
                        Text(backIn).font(.system(size: 20, weight: .semibold, design: .rounded)).monospacedDigit()
                    }
                    .fixedSize()
                }
            }
            .padding(.horizontal, 12).frame(height: GaugeLayout.hero)
            .background(Color.red.opacity(contrast == .increased ? 0.18 : 0.08), in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(contrast == .increased ? Color.red : .clear, lineWidth: 1))
            .contentShape(RoundedRectangle(cornerRadius: 12))
        }
        .buttonStyle(PanelPressStyle()).focusable()
        .help(notice.help)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(notice.spoken)
        .accessibilityHint(notice.seat == nil ? "" : "Shows that seat's details")
        .accessibilityAddTraits(.isButton)
    }
}

/// No Claude seat has a fresh reading: a muted card in the place and size of "Use now". Clicking it refreshes.
private struct NoFreshCard: View {
    let notice: DirectionPresentation.NoFresh
    let busy: Bool
    let action: () -> Void
    @Environment(\.colorSchemeContrast) private var contrast
    var body: some View {
        Button(action: action) {
            HStack(spacing: 12) {
                Group {
                    if busy { ProgressView().controlSize(.small) }
                    else { Image(systemName: "arrow.clockwise").font(.system(size: 20, weight: .medium)) }
                }
                .foregroundStyle(PanelColor.secondaryText).frame(width: 48, height: 48)
                VStack(alignment: .leading, spacing: 1) {
                    Text("Use now").font(.system(size: 10, weight: .medium)).foregroundStyle(PanelColor.secondaryText)
                    Text(notice.title).font(.system(size: 15, weight: .semibold)).foregroundStyle(PanelColor.secondaryText)
                        .lineLimit(1)
                    Text(notice.detail).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                        .monospacedDigit().lineLimit(1)
                }
                Spacer(minLength: 4)
            }
            .padding(.horizontal, 12).frame(height: GaugeLayout.hero)
            .background(Color.primary.opacity(contrast == .increased ? 0.10 : 0.05), in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(
                Color.secondary.opacity(contrast == .increased ? 0.9 : 0.35), style: StrokeStyle(lineWidth: 1, dash: [3, 3])))
            .contentShape(RoundedRectangle(cornerRadius: 12))
        }
        .buttonStyle(PanelPressStyle()).focusable().disabled(busy)
        .help(notice.help)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(notice.spoken)
        .accessibilityHint("Refreshes account usage")
        .accessibilityAddTraits(.isButton)
    }
}

/// The exact command that signs a seat in again, with a Copy button. The app never runs it.
private struct SignInCommandRow: View {
    let command: String
    @State private var copied = false
    @Environment(\.displayScale) private var displayScale
    var body: some View {
        HStack(spacing: 6) {
            Text(command).font(.system(size: 10, design: .monospaced)).lineLimit(1).minimumScaleFactor(0.8).truncationMode(.middle)
                .textSelection(.enabled).help(command)
                .accessibilityLabel("Sign-in command: " + command)
            Spacer(minLength: 2)
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(command, forType: .string)
                copied = true
            } label: {
                // An icon, so the whole command fits on its line; the tooltip and VoiceOver say "Copy".
                Image(systemName: copied ? "checkmark" : "doc.on.doc")
                    .font(.system(size: 10, weight: .medium)).foregroundStyle(Color.accentColor)
                    .frame(width: 18, height: 18).contentShape(Rectangle())
            }
            .buttonStyle(PanelPressStyle()).focusable()
            .help(copied ? "Copied" : "Copy").accessibilityLabel(copied ? "Copied sign-in command" : "Copy sign-in command")
        }
        .padding(.horizontal, 7).frame(height: GaugeLayout.commandRow - 4)
        .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 5))
        .overlay(RoundedRectangle(cornerRadius: 5).strokeBorder(PanelColor.hairline, lineWidth: 1 / displayScale))
        .padding(.vertical, 2)
        .onChange(of: command) { _, _ in copied = false }
    }
}

struct VerticalFuel: View {
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

/// One account: the main fill is its room (the tighter of 5-hour and weekly), the thin bar its model's own
/// limit, then its name with a state symbol, the number left and the reset chip.
private struct GaugeSegmentButton: View {
    let seat: SeatGlyph
    let modelBar: Bool
    let selected: Bool
    let focused: Bool
    let openable: Bool
    let action: () -> Void
    @State private var hovered = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.colorSchemeContrast) private var contrast

    private var symbol: String { seat.symbol }
    private var symbolColor: Color { seat.needsSignIn ? .orange : seat.tone.color }
    private var numberIsMuted: Bool { seat.room == nil && ![.out, .balance].contains(seat.tone) }
    private var chipLine: String {
        switch seat.tone {
        case .balance: "prepaid"
        default: seat.chip ?? " "
        }
    }

    var body: some View {
        Button(action: action) {
            VStack(spacing: 0) {
                GaugeBar(seat: seat, selected: selected).frame(height: GaugeLayout.segmentBar)
                    .overlay(alignment: .topTrailing) {
                        // A reading at least 20 minutes old: a small clock mark; the age is under the number and in the tooltip.
                        if seat.clock {
                            Image(systemName: "clock.fill").font(.system(size: 7, weight: .bold))
                                .foregroundStyle(PanelColor.secondaryText)
                                .padding(1.5).background(PanelColor.canvas, in: Circle())
                                .offset(x: 3, y: -3)
                        }
                    }
                if modelBar {
                    ModelBar(left: seat.modelLeft)
                        .frame(height: GaugeLayout.modelBar).padding(.top, GaugeLayout.modelBarGap)
                }
                VStack(spacing: 1) {
                    HStack(spacing: 2) {
                        Image(systemName: symbol).font(.system(size: 8, weight: .bold)).foregroundStyle(symbolColor)
                        Text(seat.name).font(.system(size: 10, weight: seat.best ? .semibold : .medium)).lineLimit(1)
                    }
                    .frame(height: 13)
                    Text(seat.tone == .balance ? (seat.chip ?? "—") : seat.roomText)
                        .font(.system(size: seat.tone == .balance ? GaugeLayout.balanceFont : GaugeLayout.numberFont,
                                      weight: .semibold, design: .rounded))
                        .monospacedDigit().lineLimit(1).minimumScaleFactor(0.7)
                        .foregroundStyle(numberIsMuted ? PanelColor.secondaryText : Color.primary)
                        .frame(height: 18)
                    Text(chipLine).lineLimit(1).fixedSize()
                    .font(.system(size: 9, weight: seat.needsSignIn ? .semibold : .regular)).monospacedDigit()
                    .foregroundStyle(seat.tone == .out ? Color.red : seat.needsSignIn ? Color.orange : PanelColor.secondaryText)
                    .frame(height: 12)
                }
                .padding(.top, 4)
                .frame(height: GaugeLayout.labels + 4, alignment: .top)
            }
            .frame(maxWidth: .infinity)
            .background((hovered || selected) && openable ? Color.primary.opacity(selected ? 0.07 : 0.04) : Color.clear,
                        in: RoundedRectangle(cornerRadius: 6))
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(focused ? Color.accentColor : .clear, lineWidth: 2))
            .contentShape(Rectangle())
        }
        .buttonStyle(PanelPressStyle()).focusable(openable).focusEffectDisabled()
        .onHover { hovered = $0 }
        .animation(reduceMotion ? nil : PanelMotion.feedback, value: hovered)
        .help(seat.detail)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(seat.spoken)
        .accessibilityHint(openable ? (selected ? "Hides details" : "Shows every limit and the menu bar pin") : "")
        .accessibilityAddTraits(openable ? [.isButton] : [])
        .accessibilityAddTraits(selected ? [.isSelected] : [])
    }
}

/// The segment's main bar. Untrusted readings draw a dashed outline and no fill; a partial seat fills what is
/// known, lighter, inside a dashed outline. Values never animate.
private struct GaugeBar: View {
    let seat: SeatGlyph
    let selected: Bool
    @Environment(\.colorSchemeContrast) private var contrast
    private var fill: Double? {
        switch seat.tone {
        case .balance: seat.balanceEmpty ? 0 : 1
        case .out: nil
        default: seat.room
        }
    }
    private var dashed: Bool { seat.partial || (seat.room == nil && ![.out, .balance].contains(seat.tone)) }
    var body: some View {
        GeometryReader { geometry in
            ZStack(alignment: .leading) {
                RoundedRectangle(cornerRadius: 4)
                    .fill(seat.tone == .out ? Color.red.opacity(contrast == .increased ? 0.30 : 0.18)
                          : Color.primary.opacity(contrast == .increased ? 0.16 : 0.08))
                if let fill, fill > 0 {
                    RoundedRectangle(cornerRadius: 4).fill(seat.tone.color.opacity(seat.partial ? 0.6 : 1))
                        .frame(width: max(4, geometry.size.width * min(1, fill)))
                }
                if seat.tone == .out {
                    Image(systemName: "xmark").font(.system(size: 9, weight: .bold)).foregroundStyle(Color.red)
                        .frame(maxWidth: .infinity)
                }
            }
        }
        .overlay {
            if dashed {
                RoundedRectangle(cornerRadius: 4).strokeBorder(Color.secondary.opacity(contrast == .increased ? 0.9 : 0.5),
                                                                style: StrokeStyle(lineWidth: 1, dash: [2, 2]))
            }
        }
        .overlay {
            if seat.best || selected {
                RoundedRectangle(cornerRadius: 4).strokeBorder(Color.accentColor, lineWidth: 1.5)
            } else if contrast == .increased {
                RoundedRectangle(cornerRadius: 4).strokeBorder(Color.primary.opacity(0.35), lineWidth: 1)
            }
        }
        .animation(nil, value: fill)
        .accessibilityHidden(true)
    }
}

/// The thin bar under a segment: what is left of a model's own limit, for example Fable.
private struct ModelBar: View {
    let left: Double?
    var body: some View {
        GeometryReader { geometry in
            if let left {
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.primary.opacity(0.08))
                    if left > 0 {
                        Capsule().fill(modelTint).frame(width: max(4, geometry.size.width * min(1, left)))
                    }
                }
            }
        }
        .accessibilityHidden(true)
    }
}

/// One line under a gauge whose seats have a model limit: what the thin bar means, for example "Room for Fable".
private struct GaugeLegend: View {
    let model: String
    let spoken: String
    var body: some View {
        HStack(spacing: 5) {
            Capsule().fill(modelTint).frame(width: 10, height: 3)
            Text(model)
        }
        .font(.system(size: 9)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
        .frame(height: GaugeLayout.legend, alignment: .bottomLeading)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(spoken)
    }
}

/// Plans with no reading, named on one quiet line instead of empty tiles. The reasons are in the tooltip.
private struct NoReadingLineView: View {
    let line: DirectionPresentation.NoReadingLine
    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: SeatTone.unknown.symbol).font(.system(size: 9)).accessibilityHidden(true)
            Text(line.text).lineLimit(1).truncationMode(.tail)
        }
        .font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
        .frame(maxWidth: .infinity, alignment: .leading).frame(height: GaugeLayout.noReadingLine, alignment: .leading)
        .help(line.help)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(line.spoken)
    }
}

/// A seat's details, in place under its gauge: every limit with what is left, its reset and its age;
/// estimated spend; refresh errors and sign-in needs; the menu bar pin.
struct SeatDetailCard: View {
    let detail: SeatDetail
    let onPin: (String) -> Void
    let onClose: () -> Void
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 6) {
                Text(detail.title).font(.system(size: 12, weight: .semibold)).lineLimit(1)
                Text(detail.identity).font(.system(size: 10)).foregroundStyle(PanelColor.secondaryText)
                    .lineLimit(1).truncationMode(.middle).help(detail.identity).textSelection(.enabled)
                Spacer(minLength: 4)
                Button(action: onClose) {
                    Image(systemName: "xmark").font(.system(size: 9, weight: .semibold)).foregroundStyle(.secondary)
                        .frame(width: 18, height: 18).contentShape(Rectangle())
                }
                .buttonStyle(PanelPressStyle()).focusable().help("Close details").accessibilityLabel("Close details")
            }
            .frame(height: 20)
            ForEach(detail.lines) { line in
                // Widest row: title 86, meter 36, "~100% left", "reset unknown", "59m ago" and gaps fit 316 points.
                HStack(spacing: 5) {
                    Text(line.title).font(.system(size: 11)).foregroundStyle(PanelColor.secondaryText).lineLimit(1)
                        .frame(width: 86, alignment: .leading).help(line.title)
                    // A value too old to count has no meter and is muted, like its tile; "(too old)" takes the room.
                    if !line.tooOld {
                        MiniMeter(fraction: line.fraction, tint: line.modelLimit ? modelTint : nil).frame(width: 36)
                    }
                    Text(line.value).font(.system(size: 11, weight: .semibold)).monospacedDigit().fixedSize()
                        .foregroundStyle(line.tooOld ? PanelColor.secondaryText : Color.primary)
                    Spacer(minLength: 2)
                    Text(line.reset).font(.system(size: 10)).monospacedDigit().foregroundStyle(PanelColor.secondaryText).fixedSize()
                    Text(line.age).font(.system(size: 10)).monospacedDigit().foregroundStyle(.tertiary).fixedSize()
                }
                .frame(height: 22)
                .accessibilityElement(children: .ignore).accessibilityLabel(line.spoken)
            }
            ForEach(Array(detail.balances.enumerated()), id: \.offset) { _, line in
                Text(line).font(.system(size: 11)).monospacedDigit().frame(height: 20)
            }
            ForEach(Array(detail.issues.enumerated()), id: \.offset) { _, issue in
                Label(issue, systemImage: "exclamationmark.triangle.fill")
                    .font(.system(size: 10)).foregroundStyle(Color.orange).lineLimit(1).frame(height: 18).help(issue)
            }
            if let command = detail.signInCommand { SignInCommandRow(command: command) }
            if let spend = detail.spend {
                GlanceCostLine(cost: spend).frame(height: 18)
                    .accessibilityElement(children: .ignore).accessibilityLabel(spend.spoken)
            }
            HStack {
                Button { onPin(detail.key) } label: {
                    Label(detail.pinned ? "Pinned in menu bar" : "Pin in menu bar", systemImage: detail.pinned ? "pin.fill" : "pin")
                        .font(.system(size: 10, weight: .medium))
                        .foregroundStyle(detail.pinned ? Color.accentColor : PanelColor.secondaryText)
                }
                .buttonStyle(PanelPressStyle()).focusable()
                .accessibilityLabel((detail.pinned ? "Unpin " : "Pin ") + detail.title + " in the menu bar")
                Spacer()
            }
            .frame(height: 28)
        }
        .padding(.horizontal, 10).padding(.vertical, 6)
        .frame(height: GaugeLayout.detailHeight(detail), alignment: .top)
        .panelCard()
    }
}
