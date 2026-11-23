# Moving the window to a Sidecar iPad

**Window ▸ Move Window to iPad / Back to Mac** (also in the command palette as
`MoveWindowToSidecar`) moves the Terminal Harbor window onto an iPad that macOS
uses as a Sidecar display, and back again. It exists because dragging a large
terminal window across to the iPad is awkward, and the iPad is often used as a
dedicated terminal screen.

## Behavior

- From any non-iPad screen, the window moves to the first iPad screen.
- From the iPad, it returns to the screen with the menu bar (`NSScreen.screens`
  index 0), not to whichever screen it came from.
- On arrival the window fills that screen's visible frame (inside the menu bar
  and Dock), so the terminal resizes to the iPad's resolution.
- Without an iPad display, or while the window is in full screen, nothing
  moves and a notification says why. Full screen owns its own Space, so moving
  the frame would fight the system layout; exit full screen first.

## How the iPad is recognised

There is no public AppKit property that marks a screen as Sidecar.
`harbor_display::sidecar_toggle_target()` therefore matches the screen's
`localizedName` case-insensitively against `ipad` and `sidecar`: macOS names a
Sidecar screen after the iPad (for example `Studio iPad`), and some releases
include "Sidecar" in it. An iPad renamed to something with neither word will
not be found. Screen names are written to the log, never to the notification.

The move itself is `WindowOps::move_to_screen()` in the `window` crate. It
takes the picker as a function so that the decision stays testable in
`wezterm-gui` while the macOS implementation only enumerates screens and calls
`setFrame:display:` with the target's `visibleFrame`. Cocoa frames share one
global coordinate space, so no per-screen backing-scale conversion is needed.
Other platforms keep the default implementation, which leaves the window where
it is, and the menu item is only registered on macOS.

## Verification

- `cargo +stable test -p wezterm-gui harbor_display` covers the choice of
  target: Mac to iPad, external monitor to iPad, iPad back to the menu-bar
  screen, case-insensitive matching, and the no-iPad cases.
- On the Mac, connect an iPad with Sidecar, choose the menu item, and confirm
  the window fills the iPad; choose it again and confirm it returns. With the
  iPad disconnected, confirm the notification appears. If the iPad is not
  found, check the logged screen names (`wezterm.gui.screens()` in the debug
  overlay lists them too) against the matching words above.
