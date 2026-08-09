# 内部リリース配置ログ

`/Applications/Terminal Harbor.app` への配置記録。項目は
[`maintenance.md`](maintenance.md)「このMacでの内部リリース配置」が定めるもの。
新しい配置を先頭に追記する。

---

## 2026-08-09 13:17 — エージェント名の`argv[0]`判定と起動時CWD復元

| 項目 | 値 |
| --- | --- |
| source commit | `b39f50c28`（ブランチ `sidebar-agent-activity`、配置時点で未コミットの修正あり） |
| toolchain | rustc 1.97.1 (8bab26f4f 2026-07-14) / cargo 1.97.1 |
| ビルド日時 | 2026-08-09 09:50 |
| `wezterm-gui` | `91477a4871da5b02aa3781b3b794a3785c968275f07039d5e6b87eac22c770cf` |
| `wezterm` | `aff51e6e6d560b364f9836aeac49225a9065fcb8321f94550ba83e4e04ac0f74` |
| `wezterm-mux-server` | `fcdc3bc6ad51bf5c044a9a9230ab7ac6ec32272f9e81243f0353cbdd8b3b2acd` |
| `strip-ansi-escapes` | `f8f689d4594623c60a15405963254646e7eb4159974a351691797010c61be5ac` |
| 署名 | ad-hoc (`codesign --force --deep --sign -`)、`--verify --deep --strict` 合格 |
| 配置日時 | 2026-08-09 13:17 |
| 旧バンドル退避先 | `/private/tmp/Terminal Harbor.previous-20260809-1317.app` |
| 必要な再起動方式 | **完全再起動** (`wezterm restart --full`) |

前回配置した2件が実機で動作しなかったため、その原因を修正した。

サイドバーのエージェント判定を実行ファイル名から`argv[0]`へ変更した。Claude Codeは
`~/.local/share/claude/versions/<version>`を実行しており、実行ファイル名も
カーネルの`p_comm`もバージョン文字列になるため、`agent_label()`が一致せず
何も表示されなかった。`LocalProcessInfo::command_name()`と
`Pane::get_foreground_process_command_name`を追加し、mux serverが
`TH_PANE_PROCESS`へ送る値を`argv[0]`優先にした。

完全再起動後にアプリが開くワークスペースは、サイドバーではなく起動経路が
ウィンドウを作るため`resume_cwd()`を通らず、CWDが復元されないままだった。
`startup_cwd()`を追加して起動時の最初のペインへ渡すようにした。

Codexの作業内容が出ないのはHarbor側ではなくCodexの設定で、`[tui] terminal_title`
未設定のためタイトルを送っていないことが原因（`docs/harbor-sidebar.md`に記載）。

## 2026-08-09 08:59 — ワークスペースCWDの完全再起動復元

| 項目 | 値 |
| --- | --- |
| source commit | `b39f50c28`（ブランチ `sidebar-agent-activity`、配置時点では `AGENTS.md` と本ログがdirty） |
| toolchain | rustc 1.97.1 (8bab26f4f 2026-07-14) / cargo 1.97.1 |
| ビルド日時 | 2026-08-09 08:58 |
| `wezterm-gui` | `d00da5117bbaecdd53a6abd6756f5ff532ce2968f9f16a2adb3456be3b5267b8` |
| `wezterm` | `d43f1c54b33c587e9c009889d91d690ad8fe2a1445ad8a41934ae74ba8edad98` |
| `wezterm-mux-server` | `4f9fc6cdbd0b9a80b0a2cd6536a350aa407c2ceaa8ed782dada29d2ac338f00e` |
| `strip-ansi-escapes` | `e5ba85a11a22b10e02a4cf49e98e9ae7218ee2647a1e4b592ac61abdcc5a6f0d` |
| 署名 | ad-hoc (`codesign --force --deep --sign -`)、`--verify --deep --strict` 合格 |
| 配置日時 | 2026-08-09 08:59 |
| 旧バンドル退避先 | `/private/tmp/Terminal Harbor.previous-20260809-0859.app` |
| 必要な再起動方式 | **完全再起動** (`wezterm restart --full`) |

完全再起動前に各ワークスペースのactive paneのCWDを保存し、再起動後にそのCWDで
最初のターミナルを開く変更を配置した。アプリ変更時の完全バンドル配置を必須とする
運用ルールも`AGENTS.md`へ追記した。雛形由来で実行ファイルが参照しない旧ANGLE
ライブラリは完成バンドルから除外した。

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
