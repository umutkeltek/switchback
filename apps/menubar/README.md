# Switchback Bar

A background macOS 14+ AppKit menu bar app. Swift 6.2 or later; no external dependencies.

Build without installing or launching. Set MENUBAR_BUILD_DIR to a directory on an external volume first:

```sh
bash cli/menubar-install.sh --scratch-path "$MENUBAR_BUILD_DIR/release"
SWITCHBACK_MENUBAR_TEST_SCRATCH="$MENUBAR_BUILD_DIR/test-fixtures" swift test --package-path apps/menubar --scratch-path "$MENUBAR_BUILD_DIR/debug"
SWITCHBACK_MENUBAR_TEST_SCRATCH="$MENUBAR_BUILD_DIR/test-fixtures" python3 -B apps/menubar/Tests/test_install.py
```

The builder requires --scratch-path or SWITCHBACK_MENUBAR_SCRATCH; no machine-specific disk name is built in. Only an explicit `--install` installs under the existing owned runtime’s `apps` directory (SWITCHBACK_RUNTIME_ROOT, then SB_RUNTIME_ROOT, then the default `~/.switchback`). The installer attests the existing runtime manifest, rejects symlink or foreign-owned parents, signs and stages the new app before publication, and retains the previous owned app. A failed publication restores the previous app bytes. `--install --launch-at-login` writes an owned LaunchAgent for the next login; it does not launch anything. To roll back, quit the app and move the retained previous bundle back into place. No credentials or provider endpoints are accessed by this client.

The app invokes the installed `sb` in private user bins or standard Homebrew bins with fixed arguments. It first reads `sb limits --json`, then invokes `sb limits --refresh-if-due --json` every 60 seconds. Each request is drawn as it ends, so at launch the saved reading shows at once instead of after the slow refresh. Switchback owns collection scheduling, backoff and the cross-process refresh lock. Refresh now explicitly invokes `sb limits --refresh --json`. When a scheduled or forced refresh fails, the app runs the cheap saved read (`sb limits --json`) so the panel holds the latest saved readings. Opening the panel reads again when the last read is more than 20 seconds old, so due collectors refresh within a minute of opening. Only one client refresh runs at once: a timer tick or a panel opening during a refresh starts nothing, and the app keeps one handle to the running refresh, so quitting cancels it. Calls run off the main actor, with a 30-second saved-read timeout or 10-minute refresh timeout and a 1 MiB stdout ceiling; stderr is discarded.

Every account gets its own submenu, identified by provider, slot and label/email. All quota windows, balances and notes are retained. Account pins persist in UserDefaults. Defaults pin the first enabled account for each core provider. Each quota pin shows the smallest known remaining percentage among that account's exact generation windows; an unknown exact window makes its pin unknown. Tool quotas and unknown nonempty scopes remain visible but do not affect generation pins. Estimates (including usage_kind metadata) remain separate and never populate the pinned limit. Compact bar labels retain the full provider/slot identity in the tooltip. Percentages are never summed. Dollars are balances.

A sample is trusted for 15 minutes, with at most 30 seconds of future clock tolerance. Snapshot, account and window times are validated independently. A failed client refresh hides nothing: each window is judged by its own sample time, and the failure shows only in the footer. Invalid reset times, invalid percentages, sign-in needs and identity problems (a refresh error such as `authentication_required` or `identity_mismatch`, or an `identity_state` other than `matched` or `unbound`) show unknown remaining capacity. A passed reset time hides the window, except a 5-hour window: it has started again and counts as 100% left. Any other failed direct check hides nothing: windows that are fresh by their own sample time still show their numbers, and the seat's details say the direct check failed. A 5-hour window that has not started (0% used, no reset time) is a normal reading. This presentation is an observation, never admission authority. Source metadata is available only in Details. A row shows its reading's age, for example `12m old`, once the reading is 5 minutes old. Switchback re-reads a healthy collector every 10 minutes at most (sooner near a limit or a reset), inside the 15-minute trust window, so a reading is replaced before it stops counting.

A configured provider that no collector observes is reported as `no_usage_collector`. It is never polled and never shown as unknown capacity.

Inspect the same presentation without starting AppKit:

```sh
SwitchbackBar --print-menu snapshot.json
SwitchbackBar --print-menu snapshot.json --text
SwitchbackBar --version
```

JSON/text inspection includes account labels and emails from the supplied file. It does not print account IDs, login revisions or credentials. The UI truncates long menu labels visually, with the full text in tooltips.


OpenCode Go and OpenCode Zen have separate core groups and default pins. Go shows its generation quota; Zen shows its prepaid balance as a currency amount, such as USD 12.25. The legacy opencode provider remains separate. This client consumes normalized Switchback observations and does not interpret provider wire formats.

A balance-only account can pin one explicit finite balance with its provided currency, including a genuine zero balance. It never derives a balance from spent/limit fields, invents a currency, or combines multiple balance readings. Accounts with quota windows keep the quota summary. Snapshot and account freshness, enabled state, sign-in needs and identity problems apply to balances too. Estimated account or balance readings remain estimates. An untrusted balance pins ? while its last reading and sample date stay visible in the account menu. A dollar amount represents prepaid funds, never generation capacity.

When a core provider has explicit unknown or unavailable coverage and no account rows, its provider status appears automatically in the bar (for example, Go ? and Zen ?). This does not create an account identity or a saved account pin. Existing account pin defaults and preferences are preserved; providers without explicit coverage do not gain an automatic indicator.

The default status item is a 24-point vector icon. Options can show the existing pinned readings within a 96-point text budget using the actual menu-bar font. Crowded pin sets show as many readings as fit plus +N; all pin selections, account submenus and the full tooltip remain intact. macOS can hide status items: when the frontmost app's menus leave no room, or when the menu bar hides itself. A running PID alone is not a visibility check. The app publishes its rendered title, measured width, enabled state and PID through its own preferences for the owner status command, plus what macOS reports about the item: `item_on_screen` (its window has a size, lies on a screen and is reported visible), `item_frame`, `menu_bar_visible` and `visibility_changes`, the last twelve changes between shown and hidden with their times. `sb menubar status` reports `on_screen_visibility` from them.

macOS hides status items from the left end first when the frontmost app's menus are wide, and it places an item it has no place for at that end. Before it creates the item the app therefore writes its place, 280 points from the right end of the menu bar beside the system's own items, into its preferences (`NSStatusItem Preferred Position SwitchbackBar`, read only when the item is created), and records that it did (`statusItemPlacedPosition`). A place the owner chose by Command-dragging the item is kept; an install from before this rule whose item sits further left than the app's place gets the app's place once. An older install keeps no record of who chose its place, so one the owner had dragged left of 280 points is moved once too; drag it back and it stays. `item_preferred_position` in the diagnostics shows the stored place. A test copy with another bundle id sets `SBStatusItemAutosaveName` in its Info.plist so its item and place stay apart from the installed app's.

With the gauge look and readings shown (Options › Icon only in menu bar off), the item draws one mini bar per pinned Claude seat, coloured and filled by its room, then the number of the seat with the most room now. It stays inside the same 96-point budget; when the pinned seats do not fit, or no Claude seat is pinned, the item falls back to the icon. Preview it with synthetic input:

    SwitchbackBar --render-status snapshot.json --output gauge-item.png --theme dark --style gauge

When the item is hidden, the panel still opens, under the right side of the menu bar:

- a global shortcut, ⌃⌥⌘L by default; Options › Open panel shortcut picks another or turns it off. It uses the system hot-key service, which needs no Accessibility permission and sees no other keys;
- `sb bar open` (the same as `sb menubar open`), which starts the app first when it is not running.

`--install --launch-at-login` writes a LaunchAgent that relaunches the app after a crash, at most every 30 seconds, but not after Quit.

## Switchback window

One native window for everything `sb ai` offers, by clicking. Open it from the panel's window button, with
⌃⌥⌘W (system hot key, no Accessibility permission), or with `sb bar window` (starts the app first when it is
not running; an app older than the window says so in one line: run `sb menubar install`). ⌃⌥⌘L still opens
the panel; Options › Window shortcut turns ⌃⌥⌘W off the same way. While the window is open the app shows a Dock
icon and an Edit menu, so copy and paste work in its fields; closing it makes the app a menu bar app again, with
no Dock icon, even while the panel is open.

The window has four places (⌘1 to ⌘4):

- **Now**: each machine's power (on, asleep, waking), when it powers off when idle, who has the GPU, the
  loaded models with their memory, holds with Release, the account to use now (the panel's own card and
  rules), and anything wrong, one plain line each.
- **Models**: every name by job (text, image, video, audio, segment) with where it runs (GPU box, Mac or
  cloud), a loaded dot, its speed as a bar against its ceiling (dashed and against the fastest model of its
  job when no ceiling is saved) and a mark for saved settings. A route, a name that picks one of several
  models, carries a branch mark. Selecting a name opens the inspector: temperature and top P as sliders,
  max tokens as a stepper, thinking as Engine / On / Off and the reasoning guard as a switch; for a GPU box
  engine that takes them, context size with a "fits / won't fit in memory" bar, KV cache type and parallel
  slots. Every value shows where it comes from: engine default, saved for this model, a profile (not editable
  here) or locked. Nothing saved means the engine decides, and the window shows only what it knows: the
  engine's own value from its start arguments when it has one, otherwise a dashed "Engine decides" slider
  track with no handle (a click or drag on it starts a value), "auto" on a stepper until it is pressed, and
  no context handle or fit verdict until a size is chosen or known. Changes wait for Apply; Review shows each
  change from and to (resetting a locked setting "also removes the lock"), whether the model reloads, and the
  exact commands; Escape asks before unsaved changes are discarded. History offers Undo to here for every
  earlier version (never on the Mac: a revert would also put back its load settings); Load, Unload, Try it and
  the speed report's findings in plain words (with "Use this" for a suggestion the inspector can show) sit
  beside it. The "loaded" count counts each model in memory once, however many names reach it; a listed model
  that is not in memory is "Resting".
- **Machines**: a GPU box's state, uptime and GPU, its holds, Keep on (1 hour by default, menu for longer)
  and Power off. Power off is disabled, with the reason, while the box has been up under 4 minutes, while
  work runs or waits, while a hold is active, or while someone else uses it. The Mac is show-only: its
  loaded models and memory, never wake, sleep, power off, release, load settings or Undo to here; Try it on
  a Mac model says the Mac loads it if it is resting.
- **Accounts**: the panel's gauge, drawn at window width, with the same rules.

The window holds no keys and calls no HTTP. It runs the installed `sb` with exact arguments (no shell):

| Reads (about every 10 s while the window is visible; paused when hidden, minimized or covered) |
| --- |
| `sb ai status --json`; `sb ai engines --json` (every 60 s instead while every GPU box is known to be asleep); `sb ai doctor --json` every 30 s; `sb ai models --json` every 60 s |
| `sb ai settings NAME --json` for the open model; `sb ai settings MODEL --history --json` once when it is picked and after a write |
| every text and audio name's settings every 5 min, beside the reads above so the status keeps refreshing |

Picking a model reads it once the pick has held for 300 ms; picking another stops a read still running, and its
`sb` process. Hiding the window stops the reads too, and a read stopped that way is no gateway error. None of
the reads wakes a machine: the gateway answers most from what it already knows, and the engines read may check
an engine's health. Each write is one command: `sb ai set`, `unset`, `revert`, `load`, `unload`, `hold`,
`release`, `on`, `off` and `sb ai chat --json -- MODEL "prompt"` for Try it (options first and `--` before the
model, so a prompt such as "-h" is sent as text). Every action button has a small terminal button beside it:
hover shows the exact command, click copies it. A button shows a spinner while it runs, then one result line,
or one plain line with the reason `sb` gave. A button that would wake the GPU box says so ("Load now · wakes
the GPU box") and asks once; Power off asks too.

Try it always reaches the gateway, which wakes a sleeping GPU box for it. It asks first ("Send · may wake the
GPU box") whenever any model behind the name (a route picks any of them) is on a GPU box the window does not
know to be on, or when it does not know the name's models yet. The window does not know a GPU box is on when
its last status read began more than 15 s ago, when that read failed, when the window just came back from
hidden and has not read it again, or when the idle power-off is under 30 s away; Keep on then asks too and
keeps the box on with a wake. The check runs again at the click, so a reading that went stale since the button
was drawn still asks. Load, settings and holds never wake without `--wake`, which the gateway enforces.

The gateway names machines; the window calls one it wakes and powers off "GPU box" and an always-on one
"Mac". Uptime comes from the gateway when it reports `up_since_ms`; otherwise from the newest successful wake
among the machine's engines, shown as approximate ("~12 min"): it may be later than the machine's start, never
earlier, so Power off waits 4 minutes after it, with the reason. With neither, uptime is unknown and the
gateway's own check decides. The rules live in `ControlPresentation`, tested without AppKit.

Render a place offscreen, with no window on screen and no command run, from one file of saved answers
(`{"now", "status", "models", "engines", "doctor", "settings": {NAME: …}, "history": {MODEL: …}, "limits",
"selected"}`, each the JSON the matching command prints; `"status_age": SECONDS` renders an old reading):

    SwitchbackBar --render-window window.json --place models --theme dark --output models.png

`--scale 1`, `--width W --height H` and, for a file with a saved `draft`, `--review` are optional. The
synthetic files in `Tests/ControlPresentationTests/Fixtures` use neutral names; a render of real answers
shows real model and machine names, so keep it private.

## Native account panel

Click the Switchback symbol for the native panel. Its default look is the gauge, described below. In the ledger and card looks, Provider filters and account search replace nested menus; the pin in each account's expanded row (beside each card in the card look) updates its existing selection immediately. Two primary generation windows stay visible with remaining percentages and reset times. Model-only provider limits are supported. More limits, tools quotas, observation dates and reading details expand in place. OpenCode Go quota and Zen prepaid funds stay distinct. Search receives focus on every open. Tab reaches the provider filters and account controls. Escape dismisses the transient panel; Command-R refreshes. Right-click the status item for refresh and quit. Options includes an icon-only menu bar mode.

The panel reuses the existing Presenter trust predicates. Stale, reset-passed, disabled, sign-in-needed and estimated readings never draw measured capacity bars. Last readings retain their age and estimate provenance. Same-name model windows have distinct stable identities and complete accessibility descriptions. No collector, account-selection or admission policy changes.

Render the actual SwiftUI panel offscreen, with no window activation or desktop capture:

    SwitchbackBar --render-panel snapshot.json --output panel.png --theme light --provider claude

The image includes supplied account labels and emails. Use synthetic input for shareable previews.

## Gauge

The default look. It answers one question first: which seat to use now.

- **Use now.** A card at the top names the Claude seat with the most room now (see Most room now), with its number, its reset chip and, when the reading is at least 5 minutes old, its age; a seat chosen on its weekly room alone says `weekly only`. When another seat is best for a model with its own limit, such as Fable, that seat sits on the card's right; when no seat has room for that model, the right side says `No room` and names the seat that is back first, for example `Seat 2 back in 1d 5h`. When the best seat for that model is the seat on the left, the right side shows only its number, so a name is said once. A seat with no room is never named: when every seat with a reading is at its limit, the card says `No seat has room` and `Seat 3 is first back`, with `Back in 8h 29m` on its right. When no Claude seat has a fresh reading, a muted card in the same place and size says `No fresh reading` and `Newest 25m old · click to refresh` (`See the seats below · click to refresh` when the readings are recent but none counts, for example while seats need sign-in); clicking it refreshes. Long names give way in the middle, once; the number never does, and the card's tooltip has the full names. Click the card for the seat's details.
- **One gauge per provider.** A provider with several accounts gets its own group; single-account plans share `Other plans`, named by short provider names (Codex, Z.ai, MiniMax, DeepSeek, Neural, OpenR., Go, Zen) with the full name in the tooltip. A single plan with nothing to show (no windows, no balance and no error the owner can act on) gets no tile: one quiet line under the grid names them all, for example `No reading: MiniMax, Go, Zen, OpenRouter`, with the full names and reasons in its tooltip. A group wraps into rows of at most five segments, all the same width: eight plans are two rows of four. Numbers, balances and reset chips are never truncated.
- **One segment per account.** The segment's fill is its room: the tighter of its 5-hour and weekly windows. Under it are the account's name with a state symbol, the number left and a reset chip such as `↻ 4d 19h`; an account at its limit shows what is really left (for example `3`) with the used-up symbol and red state, and says when it is back, for example `back 1h 31m`. A seat that needs sign-in says `sign in`; one where another account is signed in says `account changed`; both have an orange person symbol, not the `?` of a seat with no reading; one with no usable reading says `no reading` (an unknown signed-in identity is no reading, never `account changed`), with the age of its last reading in the tooltip and the details. A reading older than the 15-minute trust window shows its last number, muted, with a clock beside its name and its age under it, for example `40m ago`; a muted number never counts as room or for Use now, and its details show the same number marked too old, for example `15% left · 26m ago (too old)`. The tooltip shows both windows and any model limit.
- **Partial seats.** When some windows have a fresh reading and others do not, the segment shows the room of the windows that are known, lighter and inside a dashed outline, with a half-filled symbol. A seat whose 5-hour window has no reading at all (the vendor sent none, as for an idle seat) has its weekly room: the tile says so (`weekly only` in the tooltip and VoiceOver), and it can be the seat to use now. Any other partial seat never counts as the seat to use now. VoiceOver and the tooltip say which window is unknown.
- **Model limits.** When accounts have a model's own limit, such as Fable, a thin purple bar under each segment shows the room usable for that model on that seat: the smaller of the model's own limit and the seat's room, the number `For Fable` on the card uses. One legend line under the gauge says `Room for Fable`. No model limit, no thin bar and no legend; when no seat's thin bar has any fill, the thin bars and the legend are left out too.
- **Details in place.** Click a segment, or press Return on a focused one, to open its details under the gauge: 5-hour, weekly and model limits with what is left, the reset time and the reading's age; other windows; prepaid balances; estimated spend; refresh errors and sign-in needs; for a seat that needs sign-in, the exact command, for example `sb account login --provider claude --slot seat2`, with a Copy button (the app never runs it); and the menu bar pin. Escape, the close button or a second click closes them.
- **Find and filter.** The magnifier (Command-F) opens account search; the filter button picks a provider or pinned accounts only, and the chosen provider is remembered. Command-R refreshes. Arrow keys move between segments; Tab reaches every control.
- **Footer.** It says when the readings were last checked, for example `Checked 5m ago`; while a refresh runs it adds `· refreshing` instead of replacing it. A check is not a reading: `sb` can finish cleanly while every collector failed, so once the newest Claude reading (or, with no Claude seat, the newest reading) is older than the 15-minute trust window the footer says `Readings 25m old` with an orange dot. After a failed refresh it says so and keeps the numbers: `Couldn’t update · showing readings from 3m ago`, aged the same way, with the error in its tooltip.
- **Accessibility.** Every segment has one VoiceOver sentence, for example `Seat 2: 38 percent left, weekly; room for Fable 27 percent; resets in 4 days 19 hours`. The menu bar gauge item has its own label, for example `Switchback limits. Use now: Seat 2, 64 percent left.` Colour never carries state alone: each state has its own symbol. Reduce Motion turns off the details transition, Reduce Transparency draws a solid background and increased contrast strengthens tracks and outlines.

The panel fits its content, down to the header, one section and the footer, keeps the 600-point height cap and scrolls beyond it. Render a seat's details offscreen with `--open-seat`, for example `--open-seat claude:seat2`. The rules live in `Presenter.directions`, `Presenter.seatDetail`, `GaugeLayout` and `StatusGauge` in `UsagePresentation`.

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

The Claude account with the most room right now is the tinted row labelled `Most room now`; the card look and the status item tooltip carry the full line, for example `Most room now: seat2 · 71% left`. An account's room is the smaller remaining percentage of its 5-hour and weekly windows. Both windows must be fresh, trusted, measured readings under the same Presenter predicates as the bars: stale, disabled, sign-in-needed and estimated accounts never count, and an untrusted window takes the account out. Two rules are shared by the tile, the best seat and the status item: a 5-hour window whose reset time has passed counts as 100% left, and a seat whose 5-hour window has no reading at all has its weekly room, marked `weekly only` (`Most room now: seat2 · 95% left (weekly only)`). A failed direct check alone does not take an account out while its windows are fresh. Ties go to the lower slot name. When no account qualifies, the line says `Most room now: no fresh Claude reading` instead of guessing. A seat with no room (a window at 95% used or more, the rule that marks a segment used up) is never named: when no seat with a reading has room, the line says so with the seat that is back first, for example `Most room now: no seat has room · seat3 back in 8h 29m`. With no seat recommended, the menu bar gauge item shows `—`, and `--render-status` does the same.

Some models have their own weekly limit, for example Fable. For that model an account's room is also capped by that limit, so an account with 70% left overall but 27% of its Fable limit left is not the best Fable seat. When the best seat or its room differs for a model, a second line names it, for example `Most room for Fable: seat2 · 64% left`; the tooltip carries both lines. A stale or untrusted model reading takes the account out for that model only. The rule lives in `Presenter.bestSeat` in the `UsagePresentation` library. It is observation, not account selection.

## Ledger

The panel draws one row per account. The columns are `5-hour left`, `Weekly left` and `Resets`, and the headers appear once at the top: every percentage is what is left, never what is used. The `Resets` column shows when the tighter window resets. Only 5-hour and weekly windows go in the columns. A model-scoped pair names its model under the account name (for example `General model`), and tool quotas or windows of another length get their own labelled line, for example `Tools · Mcp Monthly: 100% left · ↻ 20d 0h`. When the columns are account-wide, a model's own weekly limit comes first, for example `Fable · Weekly: 27% left · ↻ 4d 0h`, and a used-up model limit reads `Fable at limit` instead of `At limit`. Prepaid balances share one compact list, and providers without an account reading share one line each.

Rows use a short name: the account's label when it has a real one, without a word that repeats the provider; otherwise the slot without a trailing `--configured` and a provider prefix, so `zai-coding--configured` reads `Coding`. Two accounts that would share a short name show their slots. Tooltips, VoiceOver labels and the expanded row keep the full slot. The rule is `Presenter.shortName` in `UsagePresentation`.

The ledger keeps the trust rules: only fresh, measured readings draw bars or count as room. The account named by `Most room now` is the tinted row. An account with a trusted window at 95% used or more shows when it is back, for example `Back in 40m`: the latest reset among its windows that are out. An out window without a reset time says so. A run-out line appears only when the average use since a window began, carried forward, runs out before that window resets and less than 24 hours from now. It needs a known window length (5-hour, weekly or `window_minutes`) and at least 10% of the window gone. The snapshot holds one sample per window, so this is not a recent-rate forecast. Emails appear only in an expanded row. Rows expand on click or keyboard and hold every limit, reading details and the menu bar pin.

Optional cost per account appears as one line, for example `Spend ~$6.42 today · ~$71.95 in 7 days`, only when the snapshot carries it:

    "cost": {"currency_code": "USD", "basis": "estimated_from_local_logs", "sampled_at": "2026-01-01T12:00:00Z",
             "today": {"amount": 6.42, "tokens": 2632200}, "last_7_days": {"amount": 71.95, "tokens": 28420250}}

The bar never computes cost. It draws only the windows a row sent, so a row with only `today` reads `Spend ~$714 today`. Amounts stay short enough to fit the row: under 100 with two decimals ($6.42), up to 9,999 in whole units with a thousands separator ($714, $1,099), from 10,000 with one decimal and k ($12.0k). Amounts show only while `sampled_at` is fresh and from the same local day. A missing currency or a 7-day amount below today's amount hides the line; a sent amount that is negative or not finite shows as `—`, and hides the line when it is the only one. A malformed cost never breaks the rest of the snapshot. `~` marks an estimate, as it does for estimated quota readings. The rules are `Presenter.costGlance` and `Presenter.money` in `UsagePresentation`.

A provider with an entry in the top-level `cost_summary` gets one total line under its section header, for example `Est. spend ~$2,267 today · ~$12.0k in 7 days`. The totals do not depend on the split by account, so they can show while rows carry no cost. The line follows the same trust rules; a sent amount that cannot be shown, or two entries for one provider, hides it. The tooltip and VoiceOver say `At API list prices, from local logs. Not a bill.` When more than 2% of a window's tokens are unattributed, that figure gets a dotted underline and the tooltip says the split by account is not known yet. No `cost_summary` means no line and no reserved space. The rule is `Presenter.costTotal`.

Options › Look picks the panel's look: Gauge (the default), Ledger, Rings or Strip. The choice is saved under the existing `panelStyle` preference, so a look saved before the gauge became the default is kept. The earlier account cards stay reachable through the preference while they remain tested code:

    defaults write dev.switchback.menubar panelStyle a       # the ledger
    defaults write dev.switchback.menubar panelStyle cards   # account cards
    defaults delete dev.switchback.menubar panelStyle        # back to the gauge

The app reads the preference on every refresh. `--render-panel` draws the gauge; add `--style a` for the ledger, `--style cards`, `--style rings` or `--style strip` for the others. `rings` draws one tile per account (weekly on the outer arc, 5-hour on the inner arc, a model's own limit as a tick) and `strip` one colour line per account under a strip of upcoming 5-hour resets. Each look leads with the seat to use now; colour always comes with a symbol, and money stays out of the main view.

## Limit notifications

Options has a `Limit notifications (80%, 95%, reset)` switch. It is off by default and remembered across launches. macOS notification permission is requested only when the switch is turned on, never at launch. When on, a pinned account's generation window posts one local notification when it passes 80% used, one when it passes 95% used, and one when a window that was above 95% resets (falls back under 80%). The first reading of a window after the switch is turned on only records its level, so switching on never floods. The last notified level per account window is kept in preferences, so the next refresh or a relaunch does not repeat a notice. Untrusted readings change nothing. The crossing rule is `Presenter.limitNotices` in `UsagePresentation`; the app only posts its result. Inspection and render modes never touch the notification center.
