# `CreateWorkspace`

Creates a Terminal Harbor workspace rooted at the user's home directory and
switches to it. This does not change new-tab behavior: a new tab continues to
inherit the active pane's current working directory when it is available.

```lua
local wezterm = require 'wezterm'
local act = wezterm.action

return {
  keys = {
    { key = 'n', mods = 'CMD', action = act.CreateWorkspace },
  },
}
```
