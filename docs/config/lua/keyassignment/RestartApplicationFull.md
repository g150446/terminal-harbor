# `RestartApplicationFull`

Restarts the Terminal Harbor GUI and its persistent mux server. All terminal
sessions are terminated.

```lua
config.keys = {
  { key = 'r', mods = 'CMD|ALT|SHIFT', action = wezterm.action.RestartApplicationFull },
}
```

This is equivalent to `wezterm restart --full`.
