# Terminal Harbor

Terminal Harbor is an independent MIT-licensed fork of WezTerm focused on
workspace-oriented AI development. It adds a persistent project sidebar,
per-workspace agent activity, and a small terminal protocol for agent status.

The terminal engine, multiplexer, configuration language, and command-line
compatibility come from [WezTerm](https://github.com/wez/wezterm). The
workspace experience is implemented independently in this repository; no cmux
source code, artwork, or other GPL-covered implementation is included.

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
末尾のワークスペースを閉じた場合は、ひとつ前へ移動します。実行中の
プロセスがある場合は、ワークスペースを閉じる前に確認画面が表示されます。
タブが2個以上ある場合は従来どおり現在のタブだけを閉じます。

サイドバーを非表示にしてもワークスペースや実行中のプロセスは削除されません。

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
