# `RestartApplication`

Restarts the Terminal Harbor GUI while keeping terminal sessions running in
the persistent local mux server.

```lua
config.keys = {
  { key = 'r', mods = 'CMD|SHIFT', action = wezterm.action.RestartApplication },
}
```

This is equivalent to `wezterm restart`.
