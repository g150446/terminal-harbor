# `ActivateWorkspace`

Activates a Terminal Harbor workspace by its zero-based position in the
persisted sidebar order. If that position does not exist, the action has no
effect.

```lua
local wezterm = require 'wezterm'
local act = wezterm.action

return {
  keys = {
    { key = '1', mods = 'CMD', action = act.ActivateWorkspace(0) },
  },
}
```
