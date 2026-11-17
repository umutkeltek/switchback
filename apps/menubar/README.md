# Switchback Bar

A background macOS 14+ AppKit menu bar app. Swift 6.2 or later; no external dependencies.

Build without installing or launching. Set MENUBAR_BUILD_DIR to a directory on an external volume first:

```sh
bash cli/menubar-install.sh --scratch-path "$MENUBAR_BUILD_DIR/release"
SWITCHBACK_MENUBAR_TEST_SCRATCH="$MENUBAR_BUILD_DIR/test-fixtures" swift test --package-path apps/menubar --scratch-path "$MENUBAR_BUILD_DIR/debug"
SWITCHBACK_MENUBAR_TEST_SCRATCH="$MENUBAR_BUILD_DIR/test-fixtures" python3 -B apps/menubar/Tests/test_install.py
```

The builder requires --scratch-path or SWITCHBACK_MENUBAR_SCRATCH; no machine-specific disk name is built in. Only an explicit `--install` installs under the existing owned runtime’s `apps` directory (SWITCHBACK_RUNTIME_ROOT, then SB_RUNTIME_ROOT, then the default `~/.switchback`). The installer attests the existing runtime manifest, rejects symlink or foreign-owned parents, signs and stages the new app before publication, and retains the previous owned app. A failed publication restores the previous app bytes. `--install --launch-at-login` writes an owned LaunchAgent for the next login; it does not launch anything. To roll back, quit the app and move the retained previous bundle back into place. No credentials or provider endpoints are accessed by this client.

The app invokes the installed `sb` in private user bins or standard Homebrew bins with fixed arguments. It first reads `sb limits --json`, then invokes `sb limits --refresh-if-due --json` every 60 seconds. Switchback owns collection scheduling, backoff and the cross-process refresh lock. Refresh now explicitly invokes `sb limits --refresh --json`. Only one client subprocess runs at once. Calls run off the main actor, with a 30-second saved-read timeout or 10-minute refresh timeout and a 1 MiB stdout ceiling; stderr is discarded. Closing the app cancels its own call.

Every account gets its own submenu, identified by provider, slot and label/email. All quota windows, balances and notes are retained. Account pins persist in UserDefaults. Defaults pin the first enabled account for each core provider. Each quota pin shows the smallest known remaining percentage among that account's exact generation windows; an unknown exact window makes its pin unknown. Tool quotas and unknown nonempty scopes remain visible but do not affect generation pins. Estimates (including usage_kind metadata) remain separate and never populate the pinned limit. Compact bar labels retain the full provider/slot identity in the tooltip. Percentages are never summed. Dollars are balances.

A sample is trusted for 15 minutes, with at most 30 seconds of future clock tolerance. Snapshot, account and window times are validated independently. Passed/invalid reset times, invalid percentages, login requirements and refresh failures show unknown remaining capacity. This presentation is an observation, never admission authority. Source metadata is available only in Details.

Inspect the same presentation without starting AppKit:

```sh
SwitchbackBar --print-menu snapshot.json
SwitchbackBar --print-menu snapshot.json --text
SwitchbackBar --version
```

JSON/text inspection includes account labels and emails from the supplied file. It does not print account IDs, login revisions or credentials. The UI truncates long menu labels visually, with the full text in tooltips.


OpenCode Go and OpenCode Zen have separate core groups and default pins. Go shows its generation quota; Zen shows its prepaid balance as a currency amount, such as USD 12.25. The legacy opencode provider remains separate. This client consumes normalized Switchback observations and does not interpret provider wire formats.

A balance-only account can pin one explicit finite balance with its provided currency, including a genuine zero balance. It never derives a balance from spent/limit fields, invents a currency, or combines multiple balance readings. Accounts with quota windows keep the quota summary. Snapshot and account freshness, enabled state, login requirements and refresh failures apply to balances too. Estimated account or balance readings remain estimates. An untrusted balance pins ? while its last reading and sample date stay visible in the account menu. A dollar amount represents prepaid funds, never generation capacity.

When a core provider has explicit unknown or unavailable coverage and no account rows, its provider status appears automatically in the bar (for example, Go ? and Zen ?). This does not create an account identity or a saved account pin. Existing account pin defaults and preferences are preserved; providers without explicit coverage do not gain an automatic indicator.

The default status item is a 24-point vector icon. Options can show the existing pinned readings within a 96-point text budget using the actual menu-bar font. Crowded pin sets show as many readings as fit plus +N; all pin selections, account submenus and the full tooltip remain intact. macOS can hide oversized status items, so a running PID alone is not a visibility check. The app publishes its current rendered title, measured width, enabled state and PID through its own preferences for the owner status command. These diagnostics describe the created item; they do not claim that macOS has exposed it on screen.

## Native account panel

Click the Switchback symbol for the native panel. Provider filters and account search replace nested menus; the pin beside each account updates its existing selection immediately. Two primary generation windows stay visible with remaining percentages and reset times. Model-only provider limits are supported. More limits, tools quotas, observation dates and reading details expand in place. OpenCode Go quota and Zen prepaid funds stay distinct. Search receives focus on every open. Tab reaches the provider filters and account controls. Escape dismisses the transient panel; Command-R refreshes. Right-click the status item for refresh and quit. Options includes an icon-only menu bar mode.

The panel reuses the existing Presenter trust predicates. Stale, reset-passed, disabled, errored and estimated readings never draw measured capacity bars. Last readings retain their age and estimate provenance. Same-name model windows have distinct stable identities and complete accessibility descriptions. No collector, account-selection or admission policy changes.

Render the actual SwiftUI panel offscreen, with no window activation or desktop capture:

    SwitchbackBar --render-panel snapshot.json --output panel.png --theme light --provider claude

The image includes supplied account labels and emails. Use synthetic input for shareable previews.

## Compact presentation

The panel is 360 points wide and caps its height at 600 points, with native scrolling for larger account lists. The provider filter is remembered across launches; saved pins are unchanged. System fonts and SF Symbols render at the display's native scale. Rules and card borders use one physical pixel at both standard and Retina scale.

Opening, searching, provider switching and keyboard actions stay immediate. Pointer disclosure uses a 160 ms ease-out transition; hover feedback lasts 120 ms. Usage values never animate. Reduce Motion disables these transitions, Reduce Transparency uses a solid background, and increased contrast strengthens card outlines.

Offscreen rendering defaults to a deterministic 2x image. Add --scale 1 for standard resolution, or --reduce-transparency to inspect a solid-background accessibility variant. For example:

    SwitchbackBar --render-panel snapshot.json --output panel-2x.png --theme dark --provider claude --scale 2

## Outside interaction

An open panel observes outside left, right and middle mouse clicks and other-application activation. This explicit dismissal path covers desktop clicks while the menu bar application is inactive; the original click still reaches its destination. Inside interactions remain in the panel, and clicking the status icon again or pressing Escape closes it.

Observers are removed on every close and on application termination. Queued callbacks belong to one opening, so input from a closed panel cannot dismiss a reopened panel. Status diagnostics expose whether the mouse observer is registered and count outside dismissals.

Run the native event-lifecycle regression without posting input or opening windows on the desktop:

    SWITCHBACK_MENUBAR_TEST_SCRATCH="$MENUBAR_BUILD_DIR/test-fixtures" python3 -B apps/menubar/Tests/test_popover_dismissal.py

The test injects outside events and application notifications at the registration boundary, checks close/reopen races and observer cleanup, and verifies native mouse registration. It does not take over the user's screen.

## Most room now

The top of the Claude section names the Claude account with the most room right now, for example `Most room now: seat2 · 71% left`. The status item tooltip carries the same line. An account's room is the smaller remaining percentage of its 5-hour and weekly windows. Both windows must be fresh, trusted, measured readings under the same Presenter predicates as the bars: stale, reset-passed, errored, disabled, sign-in-needed and estimated accounts never count, and a missing or unknown window takes the account out. Ties go to the lower slot name. When no account qualifies, the line says `Most room now: no fresh Claude reading` instead of guessing. The rule lives in `Presenter.bestSeat` in the `UsagePresentation` library. It is observation, not account selection.

## Limit notifications

Options has a `Limit notifications (80%, 95%, reset)` switch. It is off by default and remembered across launches. macOS notification permission is requested only when the switch is turned on, never at launch. When on, a pinned account's generation window posts one local notification when it passes 80% used, one when it passes 95% used, and one when a window that was above 95% resets (falls back under 80%). The first reading of a window after the switch is turned on only records its level, so switching on never floods. The last notified level per account window is kept in preferences, so the next refresh or a relaunch does not repeat a notice. Untrusted readings change nothing. The crossing rule is `Presenter.limitNotices` in `UsagePresentation`; the app only posts its result. Inspection and render modes never touch the notification center.
