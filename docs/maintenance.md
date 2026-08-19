# Terminal Harbor 保守運用ガイド

この文書は、Terminal Harbor のビルド、配布、再起動、障害切り分け、
ロールバックに必要な情報をまとめた保守担当者向けランブックです。
ユーザー向けの再起動方法と設計上の制約は
[`restarting.md`](restarting.md)、サイドバーの表示仕様は
[`harbor-sidebar.md`](harbor-sidebar.md)も参照してください。

## プロセス構成

通常起動では、次のプロセスが別々に動作します。

| プロセス | 役割 | 保持再起動時 |
| --- | --- | --- |
| `wezterm-gui` | ウィンドウ、メニュー、サイドバー、描画 | 終了して新バイナリで再起動 |
| `wezterm-mux-server` | ペイン、PTY、シェル、Codexなどの子プロセス | 継続 |
| `wezterm` | CLIと再起動ヘルパー | 要求ごとに短時間実行 |

専用muxドメイン名は `terminal-harbor` です。ランタイムディレクトリには
次のファイルが作られます。macOSでランタイムディレクトリを取得できない場合、
既定値は `~/.local/share/wezterm` です。

| ファイル | 用途 |
| --- | --- |
| `terminal-harbor-mux.sock` | GUIと永続mux間のUnixソケット |
| `terminal-harbor-mux.pid` | 永続muxのPIDとロック |
| `terminal-harbor-control-<window-class>.sock` | 再起動制御ソケット（モード `0600`） |
| `log` | muxの標準出力・標準エラーの既定ログ |

PIDファイルの内容を使ってプロセスを終了するコードは、対象の実行ファイル名が
`wezterm-mux-server` であることを確認します。運用時もPIDだけを信用して別の
プロセスへシグナルを送らないでください。

## 再起動の選択

| 変更または状況 | 使用する操作 |
| --- | --- |
| GUI、メニュー、描画、サイドバーのみの変更 | `wezterm restart` |
| 作業中のシェルやCodexを維持したい | `wezterm restart` |
| mux、PTY、プロセス検出、mux起動環境の変更 | `wezterm restart --full` |
| muxとのプロトコル互換性がない | `wezterm restart --full` |
| 永続mux導入前の版から初めて更新する | 完全終了後に新しい版を起動 |
| 状態不整合やメモリリークを疑う | `wezterm restart --full` |
| 全ワークスペースを破棄してホームからやり直す | `wezterm restart --reset-workspaces` |

アプリメニューでは **Restart GUI (Keep Sessions)**、
**Restart Sessions (Restore Workspaces)**、**Reset All Workspaces** が同じ3操作に
対応します。最初の2操作には確認画面を表示しません。セッション再起動はすべての
ターミナルセッションを終了してワークスペースを復元します。
**Reset All Workspaces** はさらにワークスペース一覧も破棄するため確認画面を表示し、
ホームディレクトリをルートとする新規ワークスペース1件だけで再開します。CLIの
`--reset-workspaces` は明示指定を同意とみなし、確認を表示しません。

保持再起動ではGUIだけが更新されます。mux、PTY、シェル、Codex、環境変数、
開いているファイル、メモリ上の状態は古いプロセスに残ります。このため
コンテキストを維持できる一方、バックエンド修正の未反映や古い不具合・資源使用も
維持されます。CLIは再起動要求の前とGUI側の実行直前にmux互換性を検査し、
安全に再接続できない場合は保持再起動を拒否します。

## ビルド前の確認

リポジトリルートで次を実行します。

```sh
git status --short --branch
cargo +stable check -p wezterm-gui -p wezterm -p wezterm-mux-server
cargo +stable test -p wezterm-gui-subcommands harbor_tests
cargo +stable test -p wezterm harbor_restart::tests
cargo +stable test -p wezterm-gui harbor_restart::tests
cargo +stable test -p wezterm-gui harbor
git diff --check
cargo +stable build --release
```

CIと同じ`cargo +nightly fmt --all -- --check`も実行します。stable rustfmtは
`.rustfmt.toml`のnightly専用設定を無視するため、整形判定には使用しません。
ただし、上流由来または作業中の既存差分が
ある場合は、今回触れていないファイルを機械的に整形して同じコミットへ混ぜないで
ください。警告とエラーを区別し、将来互換警告は依存クレート名と内容を記録します。

## macOSリリースバンドル

配布物は `assets/macos/Terminal Harbor.app` を雛形にし、次を含めます。

- `Contents/MacOS/wezterm-gui`
- `Contents/MacOS/wezterm`
- `Contents/MacOS/wezterm-mux-server`
- `Contents/MacOS/strip-ansi-escapes`
- `Contents/Resources/wezterm.sh`
- `Contents/Resources/shell-completion/`
- `Contents/Resources/terminfo/`

バンドル作成後は、開発配布なら `codesign --force --deep --sign -`、正式配布なら
適切なDeveloper IDとentitlementsを使って署名し、次を必ず確認します。

```sh
codesign --verify --deep --strict --verbose=2 "/path/to/Terminal Harbor.app"
"/path/to/Terminal Harbor.app/Contents/MacOS/wezterm" restart --help
```

実行中のアプリを更新する場合は、まず新しいバンドルを一時ディレクトリに完成させ、
署名検証後に `/Applications/Terminal Harbor.app` と入れ替えます。旧バンドルは
ロールバックが完了するまで `/private/tmp` などへ退避してください。バンドル内の
ファイルを実行中に個別上書きする方法は、版が混在するため使用しません。

入れ替え直後に保持再起動を使えるのは、実行中muxと新GUIに互換性がある場合だけ
です。永続mux導入前の版、muxプロトコル変更、またはmuxサーバー自身の修正を含む
リリースでは、復元付きセッション再起動をリリース手順に明記します。

### このMacでの内部リリース配置

内部検証用の4バイナリは次のコマンドで作成します。

```sh
cargo +stable build --release \
  -p wezterm-gui -p wezterm -p wezterm-mux-server -p strip-ansi-escapes
```

`assets/macos/Terminal Harbor.app`を一時ディレクトリへ複製し、4バイナリ、
`assets/shell-integration/wezterm.sh`、`assets/shell-completion/`を所定位置へ
配置します。terminfoは`termwiz/data/wezterm.terminfo`から`tic -x`でステージング
先へ生成します。バイナリを必要に応じて`strip`した後にバンドル全体を署名し、
署名検証が成功した完成品だけを`/Applications/Terminal Harbor.app`へ入れ替えます。
雛形に旧ANGLEライブラリなどのトップレベルdylibが残っていると、現在の実行ファイルが
参照していなくてもstrict署名検証を失敗させることがあります。`otool -L`で依存関係を
確認し、配布バイナリが参照しない旧ライブラリを完成バンドルへ混入させないでください。

配置後は次を`docs/release-log.md`に追記します。

- source commitとdirty treeの有無
- Rust toolchain、ビルド日時、4バイナリのSHA-256
- 署名方式と`codesign --verify`の結果
- 配置日時、更新前バンドルの退避先、必要な再起動方式

実行中バンドルへのファイル単位のコピーや、debug/releaseバイナリの混在は避けます。
アプリを置き換えただけでは実行中プロセスは更新されません。bridgeだけの変更なら
保持再起動で反映でき、Mac本体の再起動は不要です。

## モバイルbridgeの運用

bridgeは`wezterm-gui`内で`0.0.0.0:7780`をlistenし、
`_terminal-harbor._tcp.local.`をBonjour広告します。bridgeの変更はGUI再起動で
反映されます。`wezterm-mux-server`はbridgeを所有しないため、bridgeだけの更新で
ターミナルセッションを破棄する必要はありません。

macOSの永続状態は通常
`~/Library/Application Support/terminal-harbor/mobile-devices.json`にあります。
ここにはstable `server_id`、client ID、長期secretが含まれるため、秘密情報として
扱います。Harbor同士のペアは同じディレクトリの`paired-desktops.json`に保存します。
内容、バックアップ、QR、pair URIをログ、issue、スクリーンショット、
コミットへ含めないでください。状態ファイルの削除や再生成はserver identityを変え、
既存クライアントの再pairingが必要になるため、通常の障害対応では行いません。

更新後は次を確認します。

```sh
/usr/sbin/lsof -nP -iTCP:7780 -sTCP:LISTEN
curl --fail http://127.0.0.1:7780/v1/identity
dns-sd -B _terminal-harbor._tcp local
```

最後のコマンドは確認後にCtrl-Cで終了します。identity応答では`server_id`、API版、
`hmac-sha256-v1`を確認しますが、値を公開ログへ貼り付けません。さらに既存pairingと
新規QRの両方で接続し、Tailscale endpoint、LAN fallback、LANアドレス変更後の
mDNS再発見を確認します。mDNS候補は署名済み`/v1/identity`の`server_id`が保存値と
一致した場合だけ利用されます。

モバイルAPI契約を変更した場合はdesktopを先に配備し、GUIを保持再起動してから
mobileを更新します。workspace作成対応では、mobileから`POST /v1/workspaces`を使い、
選択中rootの既定値とMac上の既存absolute pathの両方で作成・activate・一覧更新を
確認します。relative pathまたは存在しないpathは`400`で回復可能に失敗し、既存の
workspaceやpairingを削除してはいけません。

画面履歴APIの`lines`はdesktopで1〜500行に制限し、mobileは通常300行を1秒ごとに
要求します。上限変更時はdesktop実装、mobile側OpenAPI、画面要求値、widget test、
両リポジトリの運用文書を同じリリース組で更新してください。500行を超えて増やす前に、
応答サイズ、1秒pollingの通信量、Androidのテキストlayoutとスクロール応答を計測します。

tab/workspace lifecycle APIの受け入れ確認には、破棄可能なworkspaceを使います。
2つ目のtabを作成して切替後、non-final tabだけをcloseできること、final tabのcloseが
`409`になること、workspace closeでは全tab/pane/processを終了する確認がmobileに
表示されることを確認します。DELETE要求は認証に加えてbodyの`confirm: true`が必須
です。実運用workspaceをテスト対象にせず、終了したprocessは復元不能として扱います。

input APIの受け入れ確認では、空のinstructionと`submit: true`でEnterだけが送信される
こと、`POST /v1/workspaces/{id}/key`の`up`/`down`がshell履歴または対話アプリを移動
させることを確認します。矢印キーを固定escape sequenceとして送らず、terminalのkeyboard
protocolに従う`pane.key_down`を維持します。未知のkey名は`400`で拒否します。
key endpointを使うmobileより先にAPI version 1.2.0以上のdesktopを配備し、
session-preserving restart後にport 7780のlistenを確認します。旧desktopでの`404`は
pairing不良ではないため、credential削除や再pairingを行わずdesktop更新で復旧します。

HMAC要求はMacとphoneの時計差が5分を超えると拒否されます。全endpointで同時に
認証失敗する場合は、秘密情報を表示する前に時計、`client_id`の存続、desktop/mobile
の配備順を確認します。nonce replay cacheはプロセス内状態なので、再起動を認証回避
手段として使わず、原因を特定してください。

## リリース後の受け入れ確認

保持再起動では、再起動前後のGUI PIDが変わり、mux PID、ペインID、TTYが同じで
あることを確認します。復元付きセッション再起動ではGUI PIDとmux PIDが両方変わり、以前の
セッションが残っていないことを確認します。

```sh
pgrep -fl 'wezterm-gui|wezterm-mux-server'
wezterm cli list --format json
wezterm restart
wezterm restart --full
```

加えて次の手動確認を行います。

1. シェルで `cd` した後、プロンプト表示時にサイドバーのディレクトリ名が変わる。
2. タブ・分割ペインを切り替えると、選択中ペインのディレクトリ名になる。
3. 保持再起動後もシェル、Codex、スクロールバックが継続する。
4. 復元付きセッション再起動後に新しいシェルを正常に作成できる。
5. モバイルブリッジを使用する構成では、ポート `7780` の競合がなく再接続できる。
6. `_terminal-harbor._tcp`が広告され、保存済み端末が同じ`server_id`へ再接続できる。
7. タブのクローズで確認画面が出ず、最後のタブではワークスペース行だけが削除され、
   残った行が同じ順序でサイドバーに表示され、復元付きセッション再起動後も削除した行が復活しない。

## 障害切り分け

### `wezterm restart` がGUIへ接続できない

- Terminal Harborが起動中か確認します。
- CLIとGUIが同じアプリバンドル由来か確認します。
- `--class` を指定して起動したGUIには、同じ値で
  `wezterm restart --class <class>` を実行します。
- 制御ソケットがない場合、旧版GUI、起動失敗、またはwindow class不一致を疑います。

### mux互換性エラーで保持再起動できない

これはセッション破損を避けるための正常な拒否です。作業を保存して
`wezterm restart --full` を実行します。互換性検査を無効化したり、別バージョンの
CLIから制御ソケットへ直接JSONを書き込んだりしないでください。

### 再起動要求は受理されたがGUIが戻らない

1. `pgrep -fl 'wezterm-gui|wezterm-mux-server'` でプロセスを確認します。
2. ランタイムディレクトリの `log` を確認します。
3. インストール済みバンドル内に3つのweztermバイナリが揃っているか確認します。
4. 署名を `codesign --verify --deep --strict --verbose=2` で再検証します。
5. muxが残っている場合は、同じバージョンのGUIを通常起動して再接続を試します。

ソケットやPIDファイルは実プロセスと対応している可能性があります。最初の対処として
削除しないでください。プロセスが存在しないことを確認できた場合に限り、古いソケット
を退避して通常起動します。GUIの制御ソケットは起動時に安全に作り直されます。

### サイドバーのディレクトリが更新されない

OSC 7の通知、`wezterm cli list` が返すCWD、選択中タブ・ペイン、サイドバーキャッシュ
無効化の順に確認します。詳細な取得優先順位とテスト項目は
[`harbor-sidebar.md`](harbor-sidebar.md)を参照してください。

## ロールバック

1. 作業中の内容を保存し、完全終了します。
2. 問題のあるアプリバンドルを別名で退避します。
3. 直前に動作確認済みのバンドルを `/Applications/Terminal Harbor.app` へ戻します。
4. 署名を検証して起動します。
5. 新版muxが起動済みだった場合は、旧GUIとの互換性を仮定せず復元付きセッション再起動を行います。

保持再起動中のセッションはバイナリより長く生存します。そのため、ロールバックでも
GUIだけを戻せば安全とは限りません。muxまたはプロトコルに関係する障害では、
セッション維持より整合性を優先してください。

## コード責任範囲

| ファイル | 責任 |
| --- | --- |
| `wezterm-gui/src/harbor_restart.rs` | GUI側制御ソケット、再起動準備、通常終了時のmux停止 |
| `wezterm/src/harbor_restart.rs` | CLI、互換性検査、再起動ヘルパー、セッション再起動 |
| `wezterm-gui-subcommands/src/lib.rs` | ソケット・PIDパスとドメイン名 |
| `wezterm-gui/src/main.rs` | 永続muxドメインの登録と制御サーバー起動 |
| `wezterm-mux-server/src/main.rs` | Terminal Harbor専用セッションホスト |
| `wezterm-mux-server/src/daemonize.rs` | 専用PIDファイルとdaemon化 |
| `wezterm-gui/src/commands.rs` | メニューとコマンドパレット項目 |
| `config/src/keyassignment.rs` | Luaキーアクション |

再起動プロトコルはローカルUnixソケット上の1行JSONです。保持再起動と復元付き
セッション再起動はバージョン `1`、ワークスペースリセットはバージョン `2` を使います。
フィールドは `version`、`command: "restart"`、`full`、省略可能な
`reset_workspaces` です。後者を省略した旧要求は `false` として扱います。バージョン
`1` のリセット要求は、旧GUIで復元付き再起動へ暗黙に変わることを防ぐため拒否します。
プロトコルを変更するときは
CLIとGUIを同時に更新し、旧muxとの互換性、エラー応答、ソケット権限のテストを追加して
ください。
