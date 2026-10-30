# Status

Phase 1 is complete apart from the final review (Tasks 1-12). The Fedimix UI runs end to end over `DemoBackend`; the real backend is later phases.

## What works
- `nix develop -c just build|test|clippy|fmt` on the `fedimix-core` + `fedimix-gtk` workspace (23 core unit, 3 core integration, 47 gtk unit tests).
- `nix develop -c just demo` runs the app with `--demo [--demo-scenario funded|below-target|no-saving|first-run|offline] [--demo-page ...]`; without `--demo` it exits 2.
- All pages exist: Saving and Spending tabs, Coins/coin detail/Notes, Moving to Saving, Send/Receive/Refill dialogs, Join (first run), Saving setup, Settings, About.
- `just screenshot-all` writes 48 PNGs (scenario x page, 480 and 360 wide) to `target/screenshots/`.
- `just interact-demo [flow]` runs `scripts/demo-flows/*.drive` through the in-app `--demo-drive` driver.

## Verified headlessly (sway + grim, GTK_A11Y=none, cairo renderer)
- Every page screenshots at 480x640 and 360x640 without a CRITICAL or panic (screenshot-all); a sample of PNGs was read by eye against the mockups.
- Six scripted flows pass: (1) first-run Join then tabs, Saving setup dialog; (2) tab switch, Coins and back, Moving, Exit now (queue 0.42 -> 0.22); (3) Saving Send review -> confirm -> history; (4) below-target Refill -> confirm -> balance; (5) cover-traffic toggle; (6) Settings target change then immediate Back persists (debounce flush) and survives reopen.
- Core flow: `fedimix-core/tests/demo_flow.rs` and `present::tests::demo_flow_presentation`.

## Not verified
- Real pointer/keyboard input (the driver emits clicks and sets text directly; OS-level injection does not reach GTK under headless sway), popover menu rendering by hand, hardware, dark theme, accessibility (A11y disabled), real backend.
- Header menu is opened via `action app.settings`, not by clicking the menu popover.

## Next
Final review of Phase 1.

## Needs user
Nothing.
