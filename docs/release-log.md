# 内部リリース配置ログ

`/Applications/Terminal Harbor.app` への配置記録。項目は
[`maintenance.md`](maintenance.md)「このMacでの内部リリース配置」が定めるもの。
新しい配置を先頭に追記する。

---

## 2026-08-09 08:02 — サイドバーのエージェント表示

| 項目 | 値 |
| --- | --- |
| source commit | `413e1d91d`（ブランチ `sidebar-agent-activity`、配置時点ではdirty tree） |
| toolchain | rustc 1.97.1 (8bab26f4f 2026-07-14) / cargo 1.97.1 |
| ビルド日時 | 2026-08-09 08:01 |
| `wezterm-gui` | `f067778d072bf2d95a930a81a8caf72947eabaed010469b7b929ca2a2a76b327` |
| `wezterm-mux-server` | `4f9fc6cdbd0b9a80b0a2cd6536a350aa407c2ceaa8ed782dada29d2ac338f00e` |
| 署名 | ad-hoc (`codesign --force --deep --sign -`)、`--verify --deep --strict` 合格 |
| 配置日時 | 2026-08-09 08:02 |
| 旧バンドル退避先 | `/private/tmp/Terminal Harbor.previous-20260809-0802.app` |
| 必要な再起動方式 | **完全再起動** (`wezterm restart --full`) |

muxサーバーに `TH_PANE_PROCESS` の配信を追加したため、保持再起動では反映されない。
詳細は [`restarting.md`](restarting.md)「Releases that require a full restart」。

SHA-256は署名後のバンドル内バイナリの値。`codesign`が署名を埋め込むため
`target/release`の同名バイナリとは一致しない。

## 2026-08-08 13:44 — サイドバー3行表示（GUIのみ）

| 項目 | 値 |
| --- | --- |
| source commit | `10fc6816b` + 未コミット5ファイル |
| toolchain | rustc 1.97.1 / cargo 1.97.1 |
| `wezterm-gui` | `d0ed5ae184944bed399c68eb9f9fe1340cdcb2f56b3e6956b7eee77788ccde3e` |
| 署名 | ad-hoc、`--verify --deep --strict` 合格 |
| 旧バンドル退避先 | `/private/tmp/Terminal Harbor.previous-20260808-1344.app` |
| 必要な再起動方式 | 保持再起動 (`wezterm restart`) |

この配置以前は、GUIバイナリだけをバンドル内で個別上書きした痕跡
(`.harbor-gui-backup/`) があり、4バイナリの版が混在していた。この配置で
同一ソースツリー由来の4バイナリに揃えた。個別上書きは
[`maintenance.md`](maintenance.md) が禁じている。
