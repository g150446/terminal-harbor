# `CreateWorkspace`

Creates a Terminal Harbor workspace rooted at the active pane's current
working directory and switches to it.

```lua
local wezterm = require 'wezterm'
local act = wezterm.action

return {
  keys = {
    { key = 'n', mods = 'CMD', action = act.CreateWorkspace },
  },
}
```
