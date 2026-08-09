# `RestartApplicationResetWorkspaces`

Asks for confirmation, terminates every terminal session, discards the
persisted workspace list, and restarts Terminal Harbor with one new workspace
rooted at the user's home directory.

```lua
config.keys = {
  { key = 'r', mods = 'CMD|CTRL|ALT|SHIFT', action = wezterm.action.RestartApplicationResetWorkspaces },
}
```

This is equivalent to `wezterm restart --reset-workspaces`, except that the CLI
flag is non-interactive.
