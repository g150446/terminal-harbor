# Terminal Harbor

Terminal Harbor is an independent MIT-licensed fork of WezTerm focused on
workspace-oriented AI development. It adds a persistent project sidebar,
per-workspace agent activity, and a small terminal protocol for agent status.

The terminal engine, multiplexer, configuration language, and command-line
compatibility come from [WezTerm](https://github.com/wez/wezterm). The
workspace experience is implemented independently in this repository; no cmux
source code, artwork, or other GPL-covered implementation is included.

AI agents can apply a freshly built GUI without terminating their own terminal
session by running `wezterm restart`. Use `wezterm restart --full` when mux or
PTY changes also need to be loaded; see [Restarting Terminal Harbor](docs/restarting.md).
Build, deployment, troubleshooting, and rollback procedures are documented in
the [Terminal Harbor maintenance runbook](docs/maintenance.md).

## ワークスペースの使い方

Terminal Harborを初めて起動すると、現在のターミナルセッションが最初の
ワークスペースとしてサイドバーに1件表示されます。ワークスペースは終了後も
保存され、次回起動時に同じ順序で表示されます。

### 新しいワークスペースを作る

1. 現在のターミナルで、新しいワークスペースのルートにしたいディレクトリへ
   `cd` します。
2. 左サイドバー上部の **New workspace** をクリックします。
3. Terminal Harborがそのディレクトリ名を使ってワークスペースを作成し、
   自動的に新しいワークスペースへ切り替えます。

たとえば `/Users/example/projects/my-app` をワークスペースにする場合:

```sh
cd /Users/example/projects/my-app
```

その後 **New workspace** をクリックしてください。新しいシェルも
`/Users/example/projects/my-app` から開始します。同じ名前がすでにある場合は
`my-app 2` のように連番が付きます。

### ワークスペースを切り替える

左サイドバーに表示されているワークスペース名をクリックします。選択中の
ワークスペースは背景色で強調され、右側のターミナルとタブがその
ワークスペースの内容へ切り替わります。各ワークスペースのタブ、ペイン、
実行中のプロセスは互いに独立しています。

既定のキーボードショートカットでも操作できます。

| ショートカット | 操作 |
| --- | --- |
| `⌘N` | 現在のペインの作業ディレクトリからワークスペースを作成 |
| `⌘P` | 名前とパスを検索できるワークスペーススイッチャーを表示 |
| `⌃⌘[` / `⌃⌘]` | サイドバー順で前／次のワークスペースへ循環移動 |
| `⌘1`〜`⌘9` | サイドバー順の1〜9番目のワークスペースへ移動 |
| `⌘B` | サイドバーの表示・非表示を切り替え |
| `⌘W` | 現在のタブを閉じる。最後の1タブならワークスペースを閉じる |

これらの割り当ては通常のWezTerm Luaキーバインド設定で上書きできます。
従来の新規ウィンドウは `⌘⇧N` に移動しました。タブの前後移動
`⌘⇧[` / `⌘⇧]` はそのまま利用できます。従来の `⌘1`〜`⌘9` による
タブ直接選択は、ワークスペース直接選択に置き換わります。

`⌘W`でワークスペースの最後のタブを閉じると、そのワークスペースは
サイドバーと永続状態から削除され、次のワークスペースへ自動的に移動します。
末尾のワークスペースを閉じた場合は、ひとつ前へ移動します。タブのクローズでは
実行中のプロセスがあっても確認画面を表示しません。タブが2個以上ある場合は
現在のタブだけを閉じます。

サイドバーを非表示にしてもワークスペースや実行中のプロセスは削除されません。

サイドバーの各行は次の構成です。

```text
●  harbor
  Claude
  サイドバーからワークスペースのフォルダ名を削除
```

1行目は、現在のアクティブタブ内で選択されているペインのカレントディレクトリ名
です。フルパスではなく末尾名だけを表示します。2行目と3行目はAIエージェント
（`claude` / `codex` / `opencode`）の実行中のみ表示され、エージェント名と、
作業内容を1行にまとめた要約が入ります。エージェント未実行時は1行目だけになり、
`zsh` のようなプロセス名は表示しません。各行は必ず1行に収まるよう省略されます。

作業内容の要約は `TH_AGENT_MESSAGE`（下記のステータスプロトコル）を優先し、
無い場合はエージェントがOSC 0/2で設定するペインタイトルを使います。

ワークスペース作成時のフォルダ名はサイドバーには表示されません。この名前は
作成時に一度取得されたきり `cd` やタブ切り替えに追従しないためです。名前自体は
ワークスペーススイッチャー（`⌘P`）では引き続き利用できます。

1行目を正確に `cd` へ追従させるには、シェルがOSC 7を通知する必要が
あります（標準のTerminal Harborシェル統合で有効になります）。
実装上の取得順、フォールバック、更新通知、障害切り分けは
[`docs/harbor-sidebar.md`](docs/harbor-sidebar.md)を参照してください。

## Workspace status protocol

An agent can publish status to its current pane:

```sh
wezterm workspace status --state running --agent codex --message "Running tests"
wezterm workspace status --state waiting --message "Approval required"
wezterm workspace status --clear
```

The command emits standard iTerm2-compatible user variables named
`TH_AGENT_STATE`, `TH_AGENT_NAME`, and `TH_AGENT_MESSAGE`. Existing WezTerm
configuration and CLI entry points remain compatible.

`TH_PANE_PROCESS` is reserved. The session host sets it on each pane to relay
the foreground process name to attached GUIs, which cannot inspect processes
for panes the host owns. Do not publish it from an agent; use `--agent` to
override the displayed name.

## Mobile pairing

Terminal Harbor can be controlled from the companion Flutter app
([terminal-harbor-mobile](../terminal-harbor-mobile)).

1. Start Terminal Harbor (the mobile bridge listens on LAN port `7780`).
2. In the Harbor sidebar, click **Pair mobile**. A square QR image opens in your
   image viewer and the pair URI is copied to the clipboard.
3. Scan the QR code with the mobile app over Tailscale or the same local
   network, or paste the URI into the app's **Manual** entry. The URI encodes a
   short-lived `harbor://pair?...` token; the app exchanges it for a device token.
4. From the phone: create and close workspaces, create/switch/close their tabs,
   mirror the active pane’s screen (last 60 lines, refreshed every second), and send
   instruction text to the workspace’s active pane — Send also delivers a real
   Enter key event so shells and AI agents execute the line.

The bridge speaks a small JSON HTTP API (not the WezTerm binary mux protocol).
See `openapi/harbor-mobile.yaml` in the mobile repository for the contract.

## Upstream foundation

<img height="128" alt="WezTerm Icon" src="https://raw.githubusercontent.com/wezterm/wezterm/main/assets/icon/wezterm-icon.svg" align="left"> *A GPU-accelerated cross-platform terminal emulator and multiplexer written by <a href="https://github.com/wez">@wez</a> and implemented in <a href="https://www.rust-lang.org/">Rust</a>*

User facing docs and guide at: https://wezterm.org/

![Screenshot](docs/screenshots/two.png)

*Screenshot of wezterm on macOS, running vim*

## Installation

https://wezterm.org/installation

## Getting help

This is a spare time project, so please bear with me.  There are a couple of channels for support:

* You can use the [GitHub issue tracker](https://github.com/wezterm/wezterm/issues) to see if someone else has a similar issue, or to file a new one.
* Start or join a thread in our [GitHub Discussions](https://github.com/wezterm/wezterm/discussions); if you have general
  questions or want to chat with other wezterm users, you're welcome here!
* There is a [Matrix room via Element.io](https://matrix.to/#/#wezterm:matrix.org)
  for (potentially!) real time discussions.

The GitHub Discussions and Element/Gitter rooms are better suited for questions
than bug reports, but don't be afraid to use whichever you are most comfortable
using and we'll work it out.

## Supporting the Project

If you use and like WezTerm, please consider sponsoring it: your support helps
to cover the fees required to maintain the project and to validate the time
spent working on it!

[Read more about sponsoring](https://wezterm.org/sponsor.html).

* [![Sponsor WezTerm](https://img.shields.io/github/sponsors/wez?label=Sponsor%20WezTerm&logo=github&style=for-the-badge)](https://github.com/sponsors/wez)
* [Patreon](https://patreon.com/WezFurlong)
* [Ko-Fi](https://ko-fi.com/wezfurlong)
* [Liberapay](https://liberapay.com/wez)
