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
