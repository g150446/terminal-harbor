# `ShowWorkspaceSwitcher`

Opens Terminal Harbor's searchable workspace switcher. Entries show the
persisted workspace name and root path; internal mux workspace identifiers are
not displayed.

```lua
local wezterm = require 'wezterm'
local act = wezterm.action

return {
  keys = {
    { key = 'p', mods = 'CMD', action = act.ShowWorkspaceSwitcher },
  },
}
```
