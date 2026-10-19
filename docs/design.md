# ukai design

ukai（鵜飼）は、[Agent Client Protocol (ACP)](https://agentclientprotocol.com) を話す
複数のエージェント（主にクラウド上でHTTP越しに動くもの）を、1つのデスクトップアプリから
並べて操るためのクライアントです。鵜匠が何羽もの鵜を手綱で操るイメージです。

Status: draft (2026-10-14)

## Goals

- **ACP over HTTP を一級市民にする。** クラウドで動く ACP エージェントに
  Streamable HTTP / WebSocket で接続する。ローカルの stdio エージェントも同じ抽象で扱う。
- **プロジェクトごとの設定ファイル (`ukai.toml`)** で、pane に開けるもの — 各エージェントを開くときに
  起動するコマンド（stdio）や接続先（HTTP/WebSocket）— を宣言する。タブや pane の分割は設定せず、
  ユーザーが実行時に操作して決める。
- **HTML レポートと画像の描画。** エージェントが生成したレポート（HTML）や画像を pane に表示し、
  ファイル更新に追従して再描画する。
- Rust 単一ワークスペース。UI は [Dioxus](https://dioxuslabs.com) 0.7 の desktop renderer。

## Non-goals (初期)

- エディタ機能（コード編集・LSP）。ukai はエージェントの操縦席であって IDE ではない。
- 端末エミュレータ pane。将来の pane 種別として予約だけしておく（後述）。
- ACP エージェント側の実装。ただし stdio エージェントのセッションを接続より長く保つ中継（`ukai-relay`）と、
  それを Kubernetes 上でワークスペースごとに動かす `ukai-gateway` は持つ（「クラウド実行」）。
- モバイル / Web ビルド。Dioxus なので将来の余地はあるが、初期は desktop のみ。

## 背景: ACP のトランスポート

- ACP 本体は JSON-RPC 2.0。もともと stdio 前提。
- リモート向けには RFD
  [Streamable HTTP & WebSocket Transport](https://agentclientprotocol.com/rfds/streamable-http-websocket-transport)
  が Active（2026-07 時点）。単一の `/acp` エンドポイントで:
  - `POST /acp` … client→agent の JSON-RPC（`initialize` のみ 200 + body、他は 202）
  - `GET /acp` (SSE) … connection-scoped / session-scoped の2本のストリームで agent→client
  - `GET /acp` + `Upgrade: websocket` … 代替プロファイル
  - ヘッダ `Acp-Connection-Id` / `Acp-Session-Id`。認証はトランスポート外（ヘッダ・クエリ等）。
- Rust SDK: `agent-client-protocol` 3.3 と、HTTP/WS 実装の `agent-client-protocol-http` 3.3
  （`client` feature = reqwest ベースの `HttpClient`、`server` feature = axum ベースの `AcpHttpServer`）。
  URL スキームで HTTP+SSE / WebSocket を切り替える。

ukai はこの2クレートの上に乗る。プロトコルのワイヤ実装は自前で持たない。

## Architecture

```
┌──────────────────────────── ukai (desktop process) ────────────────────────────┐
│                                                                                │
│  Dioxus UI (webview)                                                           │
│  ┌──────────┬──────────────────────────────┬───────────────────────────────┐   │
│  │ sidebar  │ pane tree                    │                               │   │
│  │ agents   │ ┌──────────────┬───────────┐ │  command palette / keymap     │   │
│  │ sessions │ │ AgentPane    │ReportPane │ │                               │   │
│  │ reports  │ │ (chat, tools │(child     │ │                               │   │
│  │          │ │  permissions)│ WebView)  │ │                               │   │
│  │          │ └──────────────┴───────────┘ │                               │   │
│  └──────────┴──────────────────────────────┴───────────────────────────────┘   │
│        ▲ Signals (UiState)                    ▲ ukai-report://... (custom proto)│
│        │                                      │                                │
│  ┌─────┴──────────────── ukai-core ───────────┴─────────────────────────────┐  │
│  │ Layout (tabs + pane trees) + Transcript per pane + ReportIndex           │  │
│  └─────┬────────────────────────────────────────────────────────────────────┘  │
│        │ SessionEvent (mpsc)                                                   │
│  ┌─────┴────────────── ukai-acp ────────────────────────────────────────────┐  │
│  │ AgentConnector ── HttpClient (http/https, ws/wss)  ── Stdio (process)    │  │
│  └─────┬─────────────────────────────┬──────────────────────────────────────┘  │
└────────┼─────────────────────────────┼─────────────────────────────────────────┘
         │ HTTP/2 + SSE / WebSocket     │ stdio
         ▼                              ▼
  cloud ACP agents                local ACP agents
  (Goose server, ukai-gateway      (claude-agent-acp, codex-acp,
   + ukai-relay per stdio agent)    gemini --experimental-acp, ...)
```

### Crates

| crate          | kind      | 役割                                                                                                                                                                                                                                                                                                                                                              |
| -------------- | --------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ukai-config`  | lib       | `ukai.toml` のスキーマ（serde 型）、読み込み、変数展開、検証。I/O 以外は純粋関数で、テストの主戦場。                                                                                                                                                                                                                                                              |
| `ukai-acp`     | lib       | エージェント接続。`AgentSpec` → トランスポート選択 → ACP client 接続、セッション操作、`SessionEvent` への正規化。gateway エージェントのレポート・ポート（とそこへのトンネル `ports`）・スケジュール・ナレッジのクライアント。UI に依存しない。                                                                                                                    |
| `ukai-core`    | lib       | アプリ状態のモデル（実行時のタブ/pane 木 `Layout`、会話ログ `Transcript`、レポートの列挙と配信 `report`、発言の Markdown `markdown`、差分とレビュー `review`、添付 `attachment`、ブラウザ pane のポート指定とプレビュー `browser`、エージェント設定の表示 `config`、パレットの照合 `palette`、スラッシュコマンドの補完 `slash`）。UI フレームワークに依存しない。 |
| `ukai-voice`   | lib       | 音声入力。マイクの録音（`cpal`）と whisper.cpp（`whisper-rs`）による端末内の文字起こし。変換（ダウンミックス・リサンプル・下書きへの差し込み）は純粋関数の `audio` に置く。                                                                                                                                                                                       |
| `ukai-app`     | bin       | Dioxus desktop UI。`ukai` バイナリ。状態は `ukai-core` を Signal で包むだけに留める。                                                                                                                                                                                                                                                                             |
| `ukai-relay`   | bin + lib | stdio エージェントを子に持ち、`/acp`（HTTP/SSE・WebSocket）で出す。セッションと履歴を接続より長く保ち、再接続に `session/load` の再生で答える。                                                                                                                                                                                                                   |
| `ukai-gateway` | bin       | Kubernetes の namespace に 1 つ。`gateway.toml` のエージェントを `/agents/<id>/acp` で出し、ワークスペースごとの Pod を用意して中の relay に中継する。                                                                                                                                                                                                            |

UI 依存を `ukai-app` に閉じ込めるのは、ロジックを headless にテストするため（Dioxus のコンポーネントテストは重い）。

## ACP 接続層 (`ukai-acp`)

### トランスポート

| `transport` | 実装                                                       | 用途                         |
| ----------- | ---------------------------------------------------------- | ---------------------------- |
| `http`      | `agent_client_protocol_http::HttpClient` (`https://…/acp`) | クラウドエージェント（既定） |
| `websocket` | 同上 (`wss://…/acp`)                                       | HTTP/2 を通せない経路        |
| `stdio`     | `agent_client_protocol::AcpAgent` (`process`)              | ローカルエージェント         |

HTTP 系は `HttpClient::builder(url).configure_http(|b| …)` で reqwest を構成し、
ヘッダ（認証）・タイムアウト・プロキシを設定ファイルから流し込む。

### Client 側 capability

ukai は ACP の "Client" なので、エージェントからの要求に応える:

- `session/request_permission` … UI にダイアログを出し、ユーザー判断を返す。設定で自動許可ルールを持てる（後述 `permissions`）。
- `fs/read_text_file` / `fs/write_text_file` … **プロジェクトルート配下に限定して**提供。
  これは手元で動くエージェント（stdio と、手元のツリーを見ているリモートエージェント）の経路。
- `terminal/*` … 初期は capability を広告しない。

`transport = "gateway"` のエージェントは別で、`fs/*` は**通らない**。間に立つ `ukai-relay` はエージェントに
client capability を広告しない（クライアントがいない間に動くターンが応答者のいない `fs/*` で止まるし、
エージェントの編集は Pod の checkout に入るべきで、手元のツリーに向けるものではない）。
Pod の中で書かれたレポートは relay の読み取り専用 HTTP から読み戻す（後述「HTML レポートと画像」）。

### セッションのライフサイクル

1. `connect(agent_id)` → `initialize`（capability 交換、認証が必要なら `authenticate`）
2. `session/new { cwd, mcpServers }` → `SessionHandle`
3. `session/prompt` を投げ、`session/update` 通知を `SessionEvent` に変換して UI へ流す
4. `session/cancel`、切断時は `session/load`（agent が `loadSession` を広告している場合）で復帰

新しいセッション（復元ではないもの）は、1 と 2 の間でエージェントの版を確かめる（`ukai_acp::updates`）。
`initialize` 応答の `agentInfo.name` が npm のパッケージ名として読めれば registry.npmjs.org の `latest` を引き
（5 秒で諦める）、`agentInfo.version` より新しければ `UpdateAvailable` を出して `session/new` を止める。
UI は「再起動して更新」か「そのまま開始」（`SessionHandle::proceed`）を選ばせる。再起動は stdio ならプロセスを
起動し直し、gateway なら `POST /agents/<id>/restart` でワークスペースの Pod を止めてつなぎ直す。新しい版が
入るかはエージェントのコマンド次第（`npx -y <package>@latest` なら起動のたびに取り直す）なので、再起動後も
同じ `latest` に届かなければ聞き直さず `UpdateMissed` を出してそのまま始める。復元するセッションは他の pane が
使っているかもしれないので確かめない。版の取得は claude-agent-acp のようにエージェント自身が npm パッケージ
名を名乗る場合だけ働き、設定は要らない。

接続 1 本に複数セッションを載せられる（RFD の connection/session 2 層構造に合わせる）。
`SessionRegistry` が `(agent_id, session_id)` をキーにハンドルを保持する。

### イベント正規化

UI は ACP のスキーマ型を直接触らない。`ukai-acp` が `SessionEvent`（`crates/ukai-acp/src/session.rs`）に畳む:
`Preparing` / `UpdateAvailable` / `UpdateMissed` / `Ready` / `AgentMessage` / `AgentThought` / `ToolCall` / `ToolCallUpdate` / `Plan` / `Commands` /
`PermissionRequested` / `FileWritten` / `ConfigChanged` / `ConfigFailed` / `TurnRunning` / `TurnEnded` / `PromptFailed` / `SteerFailed` / `Closed`。
`Preparing` と `TurnRunning` は relay / gateway の拡張通知（`_ukai/progress`、`_ukai/turn_state`）から作る。
UI からは `SessionHandle`（`proceed` / `prompt` / `steer` / `cancel` / `resolve_permission` / `set_config`）で操作する。

- `Ready` はエージェントの `promptCapabilities`（`image` / `embeddedContext`）を `PromptCaps` として運ぶ。
- モデル・推論レベル（reasoning effort）・モードなどの切り替えは ACP の Session Config Options で扱う。
  `session/new` / `load` / `resume` の応答の `configOptions` を `Ready` の `config` に、`config_option_update`
  通知と `session/set_config_option` の応答を `ConfigChanged`（常に全体を置き換える）に畳む。値は
  `select`（グループ付きも平らにして `group` を持たせる）と `boolean` を扱い、それ以外の種類は落とす。
  `boolean` は `clientCapabilities.session.configOptions.boolean` で広告する。`category`（`model` /
  `thought_level` / `mode` など）は表示のヒントとして運ぶだけ。UI は composer の下段にエージェントの並び順で
  出し、エージェントが応答で確定した値だけを表示に反映する。値が `default` の選択肢は、説明に入った実際のモデル
  （claude-agent-acp は既定が解決したモデル名を入れる）を表示し、ないときだけ選択肢の名前を出す。旧来の `modes` / `session/set_mode` は扱わない。
- モデルと推論レベルには、プロジェクト × エージェントごとの既定を持つ（`state.json` のプロジェクトの
  `defaults`。例: `"defaults": { "claude": { "model": "opus", "effort": "high" } }`）。option の id はエージェント
  固有なので、`category` が `model` / `thought_level` のものに当てる。ユーザーが pane で選んだ値がそのまま既定に
  なり、アプリを閉じている間なら手で書き換えてもよい。新規セッション（`Fresh`、または復元できず新規になった
  `Unsupported`）の `Ready` の後に、モデル → 推論レベルの順に 1 つずつ `set_config` する。モデルを変えると
  選べる推論レベルが変わるので、次の値はその応答の `ConfigChanged` で確かめる。すでにその値か選択肢にない値は
  飛ばし、エージェントが拒んだら残りは送らない。`session/load` / `resume` で続けるセッションの設定には触らない。
- `ToolCall` / `ToolCallUpdate` はツール呼び出しの `content` のうち `diff`（パス・変更前・変更後）を `FileDiff` として運ぶ。
  更新に `content` があれば差分を置き換え、なければ残す。
- `Commands` は `available_commands_update` を `SlashCommand`（名前・説明・入力のヒント）の一覧にしたもので、
  届くたびに前の一覧を置き換える。ACP にコマンド実行の専用メソッドはなく、`/name 入力` で始まる prompt がそれになる。
- `prompt` は `Prompt { text, attachments }` を取る。添付は `Attachment` の 3 種で、どれを送るかは呼び出し側が
  `PromptCaps` を見て決める（`ukai_core::attachment`）: 画像は `image`（8 MiB まで、非対応なら添付を断る）、
  `embeddedContext` があれば 256 KiB 以下の UTF-8 テキストは `resource` に埋め込み、それ以外は `resource_link`
  （`file://` URI。ベースラインなので全エージェントが受ける）。
  ACP のスキーマ変更（v2 への移行など）の影響をこの境界で吸収する。

実装上の注意（SDK `agent-client-protocol` 3.3）:

- エージェントからのメッセージは接続レベルのコールバックで「変換して転送するだけ」にする。
  コールバックは SDK のディスパッチループを占有するので、ユーザー待ちをそこでしてはいけない。
  パーミッション要求は responder をコマンドループに預け、ユーザーの回答で応答する。
- `ActiveSession::send_prompt` は prompt がエラーになるとコールバックのエラーで接続ごと落ちる。
  prompt は `prepare_request(...).on_receiving_result` で自前で送り、エラーは `PromptFailed` に変換する
  （`prepare_request` にすることで、ターン終了がそれ以前の `session/update` より後に届く順序も保証される）。
- `initialize` + `session/new` には 120 秒のタイムアウトを付ける（認証切れで無応答になるエージェント対策）。
  `_ukai/` の通知（`progress` / `heartbeat`）が届くたびに数え直すので、Pod の起動を待つ間は切れない。
  更新するかをユーザーが決めている間は数えず、決めた時点から数え直す。

### 再接続

Streamable HTTP の RFD v1 では、セッションは接続とは独立にサーバー側で生き続け、切断後は新しい接続で
`initialize` → `session/load` を送って再開する。切断中のメッセージ再送（`Last-Event-ID`）は v2 に回されて
いるので、取りこぼしは `session/load` の履歴再生で埋める。SDK の HTTP クライアントは SSE が切れると
トランスポートの失敗として扱い、自動ではつなぎ直さない。

今は「切断を UI に出す → Resume で `session/load`」まで。自動再接続（指数バックオフ）は今後。
セッションを接続より長く保つのはサーバー側の責務で、stdio エージェントについては「クラウド実行」の
`ukai-relay` が引き受ける。素の stdio エージェントにローカルでつないでいる場合、途中のターンは失われうる。

## クラウド実行（`ukai-gateway` / `ukai-relay`）

stdio の ACP エージェント（claude-agent-acp など）は 1 接続 = 1 プロセスで、上の「セッションは接続より長く
生きる」を自分では満たせない。ukai はその役を Kubernetes 上の 2 つのコンポーネントで持つ。クライアントの
ukai は ACP の WebSocket プロファイルで話すクライアントのまま（`transport = "gateway"`）で、Kubernetes を
知らない。gateway について知っているのは、pane ごとのワークスペースの選び方（後述）だけ。

```
ukai ──ws──▶ ukai-gateway（namespace に 1 つ）──ws──▶ <pod IP>:7070 ukai-relay ──stdio──▶ agent
                 │                                     （ワークスペースごとの Job の Pod）
                 └─ Job の作成・待機・置き換え
```

### `ukai-relay`

- エージェントを子プロセスに持ち、`AcpHttpServer` で `/acp` を出す。中核（`hub`）は I/O を持たない状態機械で、
  JSON-RPC の値を受けて送り先付きの出力を返す。
- Pod の中では relay が PID 1 なので、親を失ったプロセスはすべて relay に渡される。回収しないとゾンビが残り、
  デーモンになるツール（Bazel のサーバーなど）が pid ファイルの pid を「まだ生きている」と見て作り直せなくなる。
  そこで PID 1 のときは自分を同じ引数で子として起動し直し、自分は `tini` のような init として残る（`init`、
  Linux のみ）。`SIGTERM` などを子に転送し、終わった子をすべて回収し、子と同じ終了コードで終わる。Job に
  `shareProcessNamespace` を足す手もあるが、sidecar（git の資格情報を持つ）のプロセスがエージェントから
  見えるようになるので使わない。
- エージェントには `initialize` を 1 回だけ送り、各クライアントの `initialize` には記録で答える
  （`loadSession` を広告する）。
- セッションごとに `session/update` と、転送したユーザーの prompt を記録する。既知のセッションへの
  `session/load` には記録の再生で答えるので、切断中に起きたことも履歴に入る。relay が知らないセッション
  （relay の再起動後など）はエージェントに load させ、その再生を記録する。
- セッション作成時の応答（`configOptions` を含む）を記録して load に返す。`session/set_config_option` の応答と
  `config_option_update` 通知で記録の `configOptions` を差し替え、再接続しても今の設定が出るようにする。
- ターンはクライアントがいなくても続く。実行中に再接続したクライアントには `_ukai/turn_state`
  （ACP v2 の `state_update` に合わせた形: `running` / `idle` と `stopReason`）で伝える。
- エージェントからの要求（パーミッション）はクライアントが来るまで保留し、つないできたクライアントに出し直す。
- クライアントの `cwd` はクライアントの手元のパスなので、`session/new` / `load` / `resume` の `cwd` は
  relay の作業ディレクトリに置き換える。エージェントには `fs/*` も terminal も広告しない。
- `/acp` の隣に、作業ディレクトリのレポートを読み取り専用で出す（`reports`）。`GET /reports` が
  checkout の `ukai.toml` の `[reports]` に合うファイルを新しい順に（更新時刻付きで）返し、
  `GET /reports/files/<プロジェクト相対パス>` が 1 ファイルを `ETag` 付きで返す。配信範囲は
  `ukai_core::report::ReportTree` が `reports.dirs` の中だけに限る（`..`・エンコードされた `..`・
  外に出るシンボリックリンクは拒否）。設定はリクエストごとに読むので、Pod の中で `ukai.toml` を
  直せばそのまま効く。`GET /reports/tree` はアセットも含めたツリーの全ファイルを `ETag` 付きで返す
  （gateway がコピーを取るため。後述「レポートのコピー」）。`/health` 以外はトークンが必要。
- エージェントは ukai に見られていることも、`reports.dirs` に書いた HTML がユーザーのサイドバーに出ることも
  知らない。そこで `session/new` / `load` / `resume` の `mcpServers` に relay 自身の MCP サーバー
  （`ukai-relay mcp --root <作業ディレクトリ>`、名前は `ukai`。`mcp`）を足す（同名のクライアントの指定は置き換える）。
  ACP のエージェントが必ずクライアントから受け取る経路なのでエージェントの種類を問わず、checkout の
  `CLAUDE.md` / `AGENTS.md` も汚さない。MCP の `initialize` 応答の `instructions` で、レポートとは何か、この
  checkout の `[reports]`（`dirs` / `globs`、`allow_network` による制限、相対参照の範囲）を伝え、`dirs` が
  空なら `ukai.toml` への足し方を教える。ツールは `list_reports`（ユーザーが今開けるレポート）と、gateway の
  Pod ではナレッジ（後述「ナレッジ」）の 4 つ。ukai の機能をエージェントに知らせるときはここを育てる。
  手元の stdio エージェントにはまだ渡していない。
- 同じく `/ports` で Pod の中で listen しているポートとエージェントが頼んだプレビューを、`/forward/<port>` で
  そのポートへの TCP 接続 1 本を WebSocket で出す（`ports`。後述「ブラウザ pane とポート」）。MCP サーバーには
  `--previews <ファイル>` を渡し、`open_preview` ツールが頼んだプレビューをそのファイルに記録する。
- 1 セッションに付くクライアントは 1 つで、後から load したほうが引き継ぐ。
- クライアントがおらずターンも動いていない状態が `idle_timeout` 続いたら終了する。

### `ukai-gateway`

- namespace に 1 つ（`Recreate` の Deployment）。`gateway.toml` の `[agents.<id>]`（イメージ、コマンド、リポジトリ、
  Secret 参照、既存 PVC の mount、リソース、`idle_timeout`）を `GET /agents/<id>/acp` の WebSocket で出す。
- 接続が来たらそのワークスペースの Job を確保して Pod の ready を待ち、Pod 内の relay に中継する。
  待つ間はクライアントに `_ukai/progress`（状況が変わるたび）と `_ukai/heartbeat`（20 秒ごと）を送り、
  クライアントからのメッセージは溜めておく。中継が始まってからは 20 秒ごとにクライアントへ WebSocket の
  Ping を送る。前段の Ingress や IAP のプロキシは、エージェントが考えている間やユーザーが読んでいる間の
  静かな接続をアイドルタイムアウトで切るため。
- レポートは `GET /agents/<id>/reports` と `/agents/<id>/reports/files/<path>` で中継する。ワークスペースの
  決め方（`Ukai-Workspace-Scope` / `Ukai-Workspace`）とトークンは `/acp` と同じだが、**Pod は起こさない**:
  すでにある仕事を読むだけなので、動いている Pod がなければコピー（後述「レポートのコピー」）から答え、
  それもなければ 410 を返してクライアントにそう言わせる（イメージの pull を待たせない）。ファイル部分はエンコードされたまま relay に渡し、デコードは relay で
  1 回だけ行う。
- ポートは `GET /agents/<id>/ports`（一覧）と `GET /agents/<id>/forward/<port>`（WebSocket）で中継する。
  ワークスペースの決め方・トークン・Pod を起こさないことはレポートと同じ。forward はクライアントを upgrade する
  前に relay へつなぎ、relay の 502（何も listen していない）はそのまま返す。
- スケジュール（後述「スケジュール」）を `/agents/<id>/schedules` で持ち、自分で発火する。
- プロジェクト（またはリポジトリ）ごとのナレッジ（後述「ナレッジ」）を `/agents/<id>/knowledge` と、Pod 向けの
  `/workspace/knowledge` で持つ。
- `POST /agents/<id>/restart` はワークスペースの Job を消し（foreground）、次の接続で新しい Pod を立てさせる。
  ワークスペースの決め方とトークンは `/acp` と同じ。サイドカーは TERM でスナップショットを取ってから終わるので、
  作業ツリーは idle で止まったときと同じく残る。そのワークスペースのセッションはすべて切れる。
  エージェントの新しい版を入れる（イメージを pull し直す、`npx ...@latest` で取り直す）ためのもの。
- 権限は namespace の Role だけ（jobs、pods と events の参照、スケジュールのコマンド用の `pods/exec`、
  スケジュールを置く configmaps）。CRD も cluster-scoped な権限も要らない。
- pane ごとのワークスペース: ukai は gateway のエージェントを開いた pane に鍵を作って `state.json` に残し、
  接続のたびにヘッダー `Ukai-Workspace-Scope`（`shared` / `pod`）と `Ukai-Workspace`（鍵）を送る。
  `shared` はそのエージェントの全 pane で 1 つの Pod と作業ツリーを共有し、`pod` は pane ごとに Pod を分ける
  （鍵がワークスペースの識別子に入る）。既定は設定の `workspace` で、pane を開くときに選び直せる。
  同じ Pod の中で作業を分けたいときは、エージェント自身に git worktree を使わせる。
- セッションの状態は relay が持つので、gateway を再起動してもエージェントは落ちない。

### スケジュール

gateway は、エージェントごとに cron で **コマンド**か **prompt** を回すスケジュールを持つ。gateway が発火する
ので、ukai を閉じていても回る（Kubernetes の CronJob ではない。発火のたびに Pod を立てると clone と
イメージの pull を毎回待つうえ、prompt の結果のセッションを後から開けない）。

- API は `/agents/<id>/schedules`（`GET` 一覧・`POST` 追加）、`/<schedule>`（`GET`・`PUT`・`DELETE`）、
  `/<schedule>/runs`（新しい順）、`/<schedule>/run` と `/<schedule>/stop`（`POST`）。トークンは `/acp` と同じ。
  型は `ukai_acp::schedules`（クライアント `SchedulesEndpoint` も）、判定は `ukai_core::schedule`（純粋）で、
  両端が同じものを使う。
- 中身: 名前、cron（5 フィールドか `@daily` などの別名。解析と次回時刻は `ukai_core::schedule::Cron` が `time` で行う）、作った人の UTC オフセット（保存時のもの。
  夏時間には追従しない）、タスク（`command`: エージェントのコンテナで checkout から `sh -c`、`prompt`: 専用の
  セッションで 1 ターン）、有効かどうか。問題（名前なし・cron 不正・空のコマンド / prompt・オフセット範囲外）は
  全部集めて 422 で返す。アプリも保存前に同じ判定を出す。
- 発火: 15 秒ごとに `Schedules::tick`。`last_due` 以降の最初の一致が過ぎていれば 1 回だけ発火する（gateway が
  止まっていた間に何回過ぎても 1 回。走っている間に来た回は飛ばす）。保存した時点から数え直すので、過去の
  時刻では発火しない。
- 実行: スケジュールごとに固定のワークスペース鍵（`schedule-<id>`、`pod` スコープ）で Pod を確保する。毎回
  同じ Pod（idle で落ちていれば作り直し、作業ツリーは snapshot から戻る）に載るので、クライアントもその Pod
  と run のセッションを開ける。
  - コマンドは `pods/exec` で走らせ、その間 relay に WebSocket を 1 本張って Pod を idle で落とさない。
    stdout と stderr を届いた順に残す。停止は pid ファイルで `kill`。
  - prompt は relay に ACP で繋いでセッションを作り、1 ターン送る。**パーミッション要求には答えない**:
    relay は答えのない要求を後から繋いだクライアントに送り直すので、ユーザーがそのセッションを開いて答える。
    停止は `session/cancel`。
- 保存: スケジュールごとに ConfigMap `ukai-schedule-<id>`（ラベル `ukai.dev/schedule`）にスケジュールと直近
  20 件の run（出力は末尾 16 KiB）を JSON で置く。変更のたびと、走っている間は 10 秒ごとに書く。gateway の
  再起動で走っていた run は「Interrupted」にする。

### ナレッジ

規約、決めたこととその理由、ハマりどころなど、セッションを越えて残したいがリポジトリにはコミットしたくない
知識を、gateway がスコープごとに持つ。`CLAUDE.md` やドキュメントに書かせると PR のノイズになり、
エージェントが書いた未整理のメモまで履歴に残るため。

- 単位はノート（名前、1 行の説明、Markdown の本文）。名前は小文字・数字・`-` の 64 文字まで、説明は 1 行
  200 文字まで、本文は 256 KiB まで。見えない文字（ゼロ幅、双方向制御、Unicode タグ）と、秘密情報らしい
  もの（秘密鍵、GitHub / AWS / Google / Slack / Anthropic / OpenAI のトークン、JWT）は拒否する。説明は
  後続の全セッションの instructions に入り、本文も読まれるので、レビューで見えない指示（rules file
  backdoor）と漏れた資格情報がそこに残らないように。問題は全部集めて 422 で返し、アプリも保存前に同じ
  判定を出す（`ukai_core::knowledge`）。型は `ukai_acp::knowledge`（クライアント `KnowledgeEndpoint` も）。
- スコープはプロジェクト。`gateway.toml` の `[agents.<id>] knowledge = "plekti"` で名付け（小文字・数字と
  `-` `_` `.` `/`、`ukai_core::knowledge::valid_scope`）、同じ名前のエージェントはリポジトリが違っても
  同じノートを見る。複数のリポジトリにまたがるプロジェクトのため。省略したエージェントのスコープは
  リポジトリで、`workspace.git` を `ukai_core::knowledge::repository_key` で正規化したもの
  （`https://github.com/o/r.git` も `git@github.com:o/r` も `github.com/o/r`）。1 つのエージェントが
  見るスコープは 1 つで、リポジトリ単位とプロジェクト単位を重ねはしない（リポジトリ固有のことは説明に
  書かせる）。宣言を `gateway.toml` に置くのは、Pod から変えられず、エージェントが別のプロジェクトの
  ノートに触れられないようにするため。ワークスペースやブランチでは分けない。
- 保存: SurrealDB（`deploy/gateway/surrealdb.yaml` の StatefulSet、PVC 1 つ、RocksDB）。gateway は
  公式の Rust SDK で WebSocket（`UKAI_KNOWLEDGE_URL`、root ユーザーは Secret `ukai-surrealdb`）でつなぐ。
  namespace `ukai`・database `knowledge`。表 `note` が今のノートをレコード id `[scope, name]` で、
  表 `note_revision` がすべての書き込みと削除を消さずに持つ。ConfigMap にしないのは、ノートが増えたときに
  etcd を文書置き場にしたくないのと、後で全文検索やベクトル検索、ノート同士のリンクを同じ所で扱いたいため。
  SurrealDB は BUSL-1.1（DB をサービスとして売ることだけを禁じる）で、`deny.toml` で許している。出荷する
  gateway が持つのは SDK のクライアント側だけで、組み込みエンジン（`kv-mem`）はテストでしか使わない。
- 版と排他: 書き込みと削除はどれも 1 つの revision（名前ごとに 1 からの連番、削除をまたいでも数え続ける）
  になる。書き手は読んだ revision（新規は 0）を渡し、1 つのトランザクションの中で今の revision と違えば
  409 で今のノートを返す。黙った上書き（最後に書いたほうが勝つ）は起きない。削除は印を付けるだけで、
  履歴は `GET .../<name>/revisions` で読め、アプリから任意の版に戻せる。汚染されたノートを後から戻せる
  ようにするため。
- 承認: ノートは最後に人が承認した revision（`approved`）を別に持つ。人がアプリで書いたものは書いた時点で
  承認済み、エージェントが書いたものは承認待ちで、その間も前に承認された revision はそのまま残る。承認は
  アプリから（`POST /agents/<id>/knowledge/<name>/approve?revision=<n>`、最新の revision のときだけ）で、
  Pod 向けの `/workspace/...` には入口がない。人が承認したノートはエージェントからは消せない（人に
  理由を伝えさせる）。アプリはサイドバーで承認待ちに印を付け、ノートの pane で承認済みの revision との
  差分と「Approve」を出す。
- 由来: revision ごとに書き手（人 / エージェント）、エージェントなら gateway がトークンから突き止めた
  ワークスペース、書き手の申告する commit（16 進だけを残す）を持つ。
- gateway は起動時に store へつなぎ、つながるまで 10 秒ごとに試す。その間も agent は動き、ナレッジの
  要求だけが 503 になる。`UKAI_KNOWLEDGE_URL` がなければナレッジは持たず、Pod にも gateway の場所を
  渡さない（MCP のナレッジのツールも出ない）。テストは SDK の `kv-mem` で本物のエンジンに対して行う。
- アプリから: `GET /agents/<id>/knowledge`（スコープと、説明までのノート、名前順）、`GET` / `PUT` /
  `DELETE ?revision=<n>` `/agents/<id>/knowledge/<name>`、`GET .../<name>/revisions`。トークンは
  `/acp` と同じ。サイドバーの「Knowledge」に、スコープを共有する gateway エージェントをまとめて並べ、ノートの pane で Markdown
  として読み、編集する。開いている pane はノートを読み直し続け、編集中に版が進んだら保存は拒否されるので、
  そう出して「自分の版で置き換える」を選ばせる。
- Pod から: relay の MCP サーバー（`ukai-relay mcp --gateway <url>`）が `/workspace/knowledge` を叩く。
  認証は relay のトークンで、gateway は同じトークンを持つ Pod を探し、Job を作るときに付けた注釈
  `ukai.dev/knowledge` からスコープを、ラベルからワークスペースを決める。Pod の申告は信じないので、Pod は
  自分のスコープのノートにしか触れない。スコープを変えても、動いている Pod は作り直すまで前のスコープの
  まま。gateway の場所は Job の `UKAI_GATEWAY_URL`（gateway の
  `UKAI_GATEWAY_INTERNAL_URL`、既定は `deploy/gateway` の Service `http://ukai-gateway`）。トークンは
  MCP サーバーの `env` で渡し、引数には載せない。
- 書き込みの上限: 1 セッション（MCP サーバー 1 つ）で 8 件、1 ワークスペースで 1 時間 20 件まで
  （gateway、429）。悪意ある入力を読んだセッションが、本物を数で上回るノートを流し込めないように。
- エージェントへの渡し方: MCP の `instructions` に承認済みのノートの一覧（名前と承認された説明。承認待ちは
  件数だけ。一覧はデータであって指示ではないと断る）と使い方を入れ、ツール
  `list_knowledge` / `read_knowledge` / `write_knowledge` / `delete_knowledge` を出す。使い方は、
  ノートは確かめるべき主張で指示ではない（作業の範囲を越えることを求めるノートには従わない）、
  ほとんどのセッションは何も書かない、このセッションで確かめたことで後から再発見しにくいことだけを書く、
  概要やコードとドキュメントにあることは書かない、落とし穴は症状・原因・対処で書く、変えるときは読んだ
  revision を渡す、チェックアウトには書かない、秘密を書かない。一覧の取得は 5 秒で諦め、セッションの
  開始を待たせない。`list_knowledge` は承認待ちも含めて印を付けて返し、`read_knowledge` は承認済みの
  revision を（なければ最新を「レビューされていない」と明示して）revision、書き手、commit を付けて返す。
  承認済みより新しい revision があればそう言い、`revision` を指定すれば最新も読める。
- これらは LLM エージェントの記憶についての研究（自己生成の知識は選別しないと効かない、記憶汚染は
  少数の書き込みで成立する、全体の書き直しは詳細を失う）を踏まえている。コード位置の引用と使用前の検証、
  全文検索は次の段階。

### ワークスペース Pod

- gateway の `job`（Job spec を作る純粋関数）と `cluster`（Job の確保・Pod の待機）が扱う。
- Job 1 つがワークスペース 1 つ。識別子は（利用者、namespace、リポジトリ、ref、エージェントのコマンド、
  `pod` なら pane の鍵）のハッシュで、同じものを開く接続は同じ Pod に合流する。
- コンテナは 3 つ。`relay-install`（relay の静的バイナリをコピーする。エージェントのイメージは ukai を知らなくて
  よい）、`workspace` サイドカー（clone と snapshot）、`agent`（relay が PID 1）。
- GitHub の token（`workspace.github_token`）はサイドカーとエージェントの両方に `GH_TOKEN` として渡す。
  エージェントは git の credential helper を `gh` に向けてあり、自分で push して pull request を作れる。
  サイドカーは snapshot をエージェントの `.git` ではなく pristine の `.git` で作る。
- 作業ツリーは emptyDir で、PVC は作らない。エージェントの履歴（`~/.claude` など）を残したいときは既存の
  PVC を `mounts` で重ねる。
- snapshot: サイドカーが pristine の `.git` とエージェントの作業ツリーから一時 index で tree を作り、
  `commit-tree` して `refs/ukai/wip/<id>` に push する（定期的に、ターン終了ごとに、Pod の停止時に）。
  次の Pod は clone 時にそれを、記録したブランチの上の未コミットの変更として戻す。
- Pod は Pod Security の restricted に合わせる（非 root、capabilities なし、seccomp RuntimeDefault）。

`deploy/gateway` が kustomize の base、`images/runtime` が gateway と relay のイメージ（main の変更ごとに
`ghcr.io/illumination-k/ukai-runtime` の `latest` とコミットのタグで公開し、base はそれを使う）、`images/dev` が
このリポジトリを開発するエージェント用のイメージ（`mise.toml` のツール群、desktop のビルド依存、
仮想ディスプレイ入り）。

## プロジェクト設定 `ukai.toml`

### 探索と合成

1. ユーザー全体: `~/.config/ukai/config.toml`（`$UKAI_CONFIG_DIR` / `$XDG_CONFIG_HOME/ukai` で変更可）
   … 共有のエージェント定義（どのプロジェクトでも使うクラウドエージェント）と、アプリ全体に効く
   `[keybindings]` と `[voice]`（後述「UI」。プロジェクトの `ukai.toml` に書くと検証エラー）
2. プロジェクト: プロジェクトルートの `ukai.toml`（任意）… プロジェクト固有のエージェント
3. （今後）ローカル上書き: `ukai.local.toml`（gitignore 前提）… 個人の URL やトークン参照

`ukai_config::load_layered` が後勝ちで合成する: `agents.<id>` は id 単位で置換、`permissions` は
プロジェクトの規則を先に評価、`project` / `reports` はプロジェクトのもの、`keybindings` / `voice` はユーザーのもの。検証は合成後に 1 回だけ行う
（ユーザー側で定義したエージェントをプロジェクトの規則から参照できるように）。
**秘密情報はファイルに書かせない**。`${env:NAME}`（環境変数）と `${keychain:NAME}`（OS のキーチェーン、
サービス `ukai`・アカウント `NAME` の項目）の展開のみ許し、未定義なら起動時にエラーではなく
そのエージェントを「未構成」として表示する。dock や Finder から起動したアプリはシェルの環境変数を
受け継がないので、`dx bundle` した app ではキーチェーンを使う。読むのは macOS のみ（`security-framework`）で、
ほかの OS では `${keychain:...}` は未定義として扱う。項目は
`security add-generic-password -U -s ukai -a <NAME> -w`（値はプロンプトで入力）で入れる。

### スキーマ

設定ファイルが宣言するのは「何を開けるか」だけで、「どこに開くか」は持たない。
`[agents.<id>]` 1 つが「pane でこのエージェントを開くときに起動するコマンド / 接続先」に当たる。

```toml
[project]
name = "ukai"

# ---- pane で開けるエージェント ---------------------------------------------
[agents.claude]
transport = "stdio"
command = "npx"
args = ["-y", "@agentclientprotocol/claude-agent-acp"]

[agents.goose-cloud]
transport = "http"
url = "https://agents.example.com/acp"
headers = { Authorization = "Bearer ${env:GOOSE_TOKEN}" }

[agents.cluster-claude] # ukai-gateway のエージェント（「クラウド実行」）
transport = "gateway"
url = "wss://ukai.example.com/agents/claude/acp"
headers = { Authorization = "Bearer ${keychain:ukai-gateway}" }
workspace = "pod" # 新しい pane の既定: shared | pod

# ---- パーミッション自動応答（未実装） -------------------------------------
[[permissions]]
agent = "claude"
tool_kind = "read" # ACP ToolKind
decision = "allow"

# ---- レポート ------------------------------------------------------------
[reports]
dirs = [
  "reports",
] # サイドバーに一覧するディレクトリ（プロジェクトルート相対。外に出る指定は検証エラー）
globs = [
  "**/*.html",
] # 既定は HTML と画像（html, htm, svg, png, jpg, jpeg, gif, webp）
allow_network = false # true でレポートからの外部読み込み（CDN の JS など）を許す
```

環境変数 `UKAI_REPORT_DIRS`（`PATH` と同じ区切り、プロジェクトルート相対）があれば、そのディレクトリを
`reports.dirs` に足す（`ukai_config::load_layered`。外に出る指定は同じく検証エラー）。`ukai.toml` を持たない
リポジトリでも、Pod のイメージや `gateway.toml` のエージェントの `env` でレポートを出せるようにするため。

`[layouts]` / `[commands]` のような配置の宣言は意図的に持たない（未知のキーとして拒否する）。

### タブと pane（実行時）

- 画面はタブの列で、各タブは pane の分割木（`ukai-core::Layout`）。split は方向・2 つ以上の子・子ごとの
  比率を持ち、同じ方向の入れ子は親に平坦化する（比率は親の枠に按分）。不変条件は `Layout::check` と
  プロパティテストで検査している。
- pane の中身: `agent { project, agent, session }`（ACP セッション 1 本）/ `report { project, path }` / 空。
  **pane ごとにプロジェクトを持つ**ので、1 つのタブに別プロジェクトのエージェントを並べられる（herdr と同じ。
  orca は worktree ごとに配置を丸ごと切り替える方式で、混在はできない）。
- 新しい pane / タブは空で開き、プロジェクトを選んでから、その設定にあるエージェント（起動コマンド付き）を選ぶ。
  サイドバーのエージェントをクリックすると、フォーカス中の pane が空ならそこに、そうでなければ右に分割して開く。
- 操作: pane ヘッダの分割（右 / 下）・閉じる、タブバーの追加・閉じる。
- **リサイズ。** split の子の間の境界（1px、掴める幅は左右 3px ずつ広い）をドラッグすると、その両隣の子の比率だけを
  動かす（`Layout::resize`。2 つの合計のうち各 5% は残す）。ダブルクリックで両隣を等分。ドラッグ中は全面の
  オーバーレイがポインタを追い、比率は保存される。
- **移動。** pane ヘッダを掴んで 4px 動かすとドラッグになり、他の pane の上に、上下左右の端（各 25%）なら
  その側に分割して入れる（`Layout::move_pane`、同じ方向の split なら兄弟として並ぶ）、中央なら位置を入れ替える
  （`Layout::swap_panes`）。タブに落とすとそのタブの右端へ、`+` に落とすと新しいタブへ移す（`Layout::move_to_tab`）。
  pane が抜けて空になったタブは閉じる。pane id は変わらないのでセッションは切れない。
- report / browser pane のネイティブ子 WebView は DOM より手前に出るので、境界やペーンのドラッグ中は
  パレットと同じく隠す（そうしないとドロップ先の表示とマウスイベントを奪う）。
  ショートカットの既定は `mod+t` 新規タブ、`mod+d` 右に分割、`mod+shift+d` 下に分割、`mod+w` pane を閉じる、
  `ctrl+tab` / `mod+shift+]`（と逆順）でタブ移動、`mod+]` / `mod+[` で pane 移動、`mod+1`〜`mod+9` で n 番目のタブ、
  `escape` でフォーカス中の pane のターンを止める、入力欄で `enter` 送信（`mod` は macOS で cmd、それ以外で ctrl）。
  変更は後述「UI」の `[keybindings]`。
- セッション・会話ログ・入力中の下書きは pane id をキーにアプリスコープで保持する。分割やタブ切り替えで
  コンポーネントが作り直されてもセッションは切れない。pane を閉じるとセッションも終わる。
- `mod+k` でパレット（後述「UI」）。

## プロジェクトと永続化

herdr（JSON 1 ファイル）と orca（SQLite のドキュメントストア + 会話ジャーナル DB）を参考にした。

- **プロジェクト** = ルートディレクトリ + 不透明な id（`p1`, `p2`, …、再利用しない）。pane は id で参照する。
  パスを id にしないのは、後で worktree やリモートの作業ディレクトリを同じプロジェクトに束ねるため。
  サイドバーでフォルダ選択ダイアログから追加、`ukai <dir>` でも追加（引数なしなら cwd から `ukai.toml` を
  遡って探す）。閉じるとそのプロジェクトの pane とセッションも閉じる。
- **DB は使わない。** 永続化するのはプロジェクトの登録（エージェントごとのモデル・推論レベルの既定を含む）と
  タブ/pane の配置（各 agent pane の ACP セッション id を含む）だけで、`~/.local/state/ukai/state.json`（`$UKAI_STATE_DIR` / `$XDG_STATE_HOME/ukai`）の 1 ドキュメント。
  - `version` 付き。新しい版のファイルや壊れたファイル、不変条件を満たさない配置は読まずに
    `state.json.broken-<unix秒>` へ退避して空の状態で起動する。
  - 変更から 500ms デバウンスして、一時ファイル → `fsync` → `rename` → ディレクトリ `fsync` で書く。
  - 存在しないプロジェクトを指す pane は読み込み時に空にする。
- **会話ログは保存しない。** 再起動後の agent pane は「Resume session」/「New session」を出す。Resume は
  `session/load`（履歴を再送してくれる）→ なければ `session/resume`（履歴なしで継続）→ どちらもなければ新規、
  の順で試し、欠けた分は pane に注記する。
- **SQLite を入れる条件:** `session/load` を持たないエージェントの会話も残したい、横断検索、レポート索引が
  必要になったとき。そのときは orca と同じく追記専用の会話ジャーナルを別ファイルにする。
- **今後:** herdr / orca はどちらも常駐プロセスがエージェントを持ち、UI の再起動ではエージェントが死なない。
  ukai ではリモートエージェントは元々サーバー側で生きている。ローカルの stdio エージェントも `ukai-relay` を
  ローカル常駐させて WebSocket で話す形にすれば同じ扱いにできる。

## HTML レポートと画像

「レポートは HTML で見たい」がこのアプリの主要ユースケースの1つ。エージェントが書いた画像（プロットなど）も
同じ pane で見る。pane の中身は `report { project, path, source }`。`source` は `project`（既定。`path` は
プロジェクトルート相対）か `agent { agent, workspace }`（gateway エージェントのワークスペース。`path` は
その checkout のルート相対）で、`source` のない `state.json` は `project` として読む。

### 開き方

1. **サイドバー** … 選択中プロジェクトの `reports.dirs` 配下で `reports.globs` に合うファイルを新しい順に一覧
   （`ukai_core::report::list`。隠しディレクトリは除外、最大 500 件）。表示中は 2 秒ごとに再走査する。
   クリックでフォーカス中の pane が空ならそこに、そうでなければ右に分割して開く（エージェントと同じ）。
   その下に、開いている gateway エージェント pane のワークスペースごとの一覧を足す。
2. **エージェント pane** … `fs/write_text_file` で HTML / 画像が書かれたら、その行に「Open」ボタンを出す
   （手元に書かれたものなので `source = project`）。
3. **ファイル選択** … サイドバーの Reports の「+」と空 pane の「Open report or image…」。プロジェクト外の
   ファイルは開かず、pane にエラーを出す。
4. （今後）ACP コンテンツ … `session/update` 内の embedded resource / resource link で `text/html` のもの。

開いている pane はレポートの変化を監視し、変わったら再読込する（`notify` ではなくポーリング。手元のファイルは
150 ms ごとに `stat`、Pod のレポートは 2 秒ごとに `ETag` を見るだけ）。

### gateway エージェントのレポート

gateway のエージェントは Pod の中に書くので、手元を走査しても見つからない。代わりに gateway に聞く
（`ukai_acp::reports`。URL はエージェントの `/acp` を `/reports` に替えたもので、ヘッダーは接続と同じ）。

- **単位は pane ではなくワークスペース。** `shared` の pane は一覧を共有し、`pod` の pane はそれぞれ持つ。
  pane を閉じてもワークスペースが動いていればレポートは読めるので、`state.json` には pane ではなく
  エージェント id とワークスペースを残す。
- `ukai-app/src/remote_reports.rs` が、開いている gateway エージェント pane のワークスペースの一覧を
  3 秒ごとに読み直して 1 つの signal に置く。サイドバーもパレットもそれを読むので、パレットは
  ネットワークを待たずに開く。Pod が idle で落ちていれば、コピーがあればその一覧を「Saved <時刻> · not running」と、
  なければ「not running」と出す。
- pane の描画は手元のレポートと同じ（IPC ハンドラのない子 WebView、`incognito`、同じ CSP）。違うのは
  `ukai-report` プロトコルの応答がファイルの読み出しではなく gateway への fetch になるところだけで、
  ネットワークを待つので非同期ハンドラ（`with_asynchronous_custom_protocol`）で答える。
- URL のパス空間は Pod の checkout のルート。`../img/plot.png` のような相対参照はディスク上と同じに解決し、
  レポートのディレクトリの外に出たものは relay が 403 で拒否する。

### レポートのコピー（S3）

Pod は idle で落ちるので、そのままでは落ちた後のレポートが読めない（410）。スケジュールの run のように
Pod が終わる前提のワークスペースでは、成果のレポートが Pod と一緒に消える。そこで gateway が、S3 の
バケット（`UKAI_REPORTS_BUCKET`。`deploy/gateway` では RustFS）にワークスペースごとのコピーを持つ
（`ukai-gateway/src/archive.rs`）。

- **読み出しの API は変えない。** Pod が動いていれば今まで通り relay から（書き換えに 2 秒で追従）、
  動いていなければ gateway がコピーから答える。コピーからの一覧には `synced`（コピーを最後に更新した時刻）が
  付き、サイドバーは「Saved <時刻> · not running」と出す。コピーもなければ 410。
- **コピーするのは gateway。** ready なワークスペース Pod の relay から 30 秒ごとに `/reports/tree` を読み、
  版（`ETag`）が変わったファイルだけを取り直して置き、消えたファイルは消す。restart で Pod を止める前にも
  1 回取る。relay の idle 終了はターンが終わって `idle_timeout` 経ってからなので、その間に追いつく。
- **資格情報は gateway だけが持つ。** Pod には渡さない（エージェントの書いたものは信頼しないので、
  バケットを触らせない。git の資格情報をサイドカーだけが持つのと同じ考え）。relay の読み取り専用の
  ルートを経由するので、配信範囲（`reports.dirs` の中だけ）も relay の判定のまま。
- バケットの中は `<ワークスペース id>/manifest.json`（relay の一覧と、コピーした各ファイルの版）と
  `<ワークスペース id>/files/<checkout 相対パス>`（content type 付き）。コピーからの読み出しもパスを
  1 回だけデコードし、`..` などの名前でない区切りは拒否する。
- 接続は `object_store`（標準の `AWS_*` 変数。`http://` のエンドポイントはそのまま許す）。起動時に
  バケットがなければ作る（RustFS は空で始まるため。作れない資格情報なら警告だけ）。1 ファイル 32 MiB、
  1 ワークスペース 2000 ファイルまで。期限は持たないので、古いワークスペースのコピーはバケットの
  lifecycle ルールで消す。

### 描画方式

- レポートは**信頼しない**入力として扱う（エージェントが生成した任意の HTML/JS）。
- **Spike の結論: アプリの WebView 内の iframe では隔離できない。** wry は IPC のメッセージハンドラを
  全フレームに公開し（macOS では `window.webkit.messageHandlers.ipc`）、Dioxus の IPC ハンドラは送信元フレームを
  見ないので、`sandbox` 付き iframe からでも UI イベントを偽造できる（パーミッション要求への回答など）。
- そこで report pane ごとに **IPC ハンドラを持たない子 WebView**（`wry::WebViewBuilder::build_as_child`）を
  作り、pane 本体の矩形に重ねる（`ukai-app/src/report_pane.rs`）。位置はポーリングで追従し、コンポーネントの
  アンマウント（タブ切り替え・分割・閉じる）で破棄する。`incognito` なので保存領域はレポート間でも残らない。
  - wry は IPC ハンドラがなくても `window.ipc` のシムを（再定義不可で）注入する。呼んでも届かないので
    安全だが、レポートがトップレベルで `ipc` という名前の `const` / `let` を宣言すると構文エラーになる。
  - 子 WebView はネイティブのビューなので DOM より手前に出る。pane に重なるポップアップは作らないこと。
    子 WebView にフォーカスがある間はアプリのショートカットが効かない。
- 子 WebView は `ukai-report://localhost/…`（Windows では `http://ukai-report.localhost/…`）だけを読み、
  `ukai_core::report::ReportFile::serve`（手元）か gateway からの fetch（Pod の中）が答える。
  - 配信範囲はレポートの**ベース**: レポートを含む `reports.dirs` のディレクトリ、なければレポートのディレクトリ。
    `../img/plot.png` のような相対参照はベース内なら通り、`..`・エンコードされた `..`・ベース外へのシンボリック
    リンクは拒否（`fs/*` と同じく正規化後のパスで判定）。
  - 画像はそれを pane に収めて表示する小さなページ（クリックで等倍）で包む。
  - 応答には `Cache-Control: no-store` と `X-Content-Type-Options: nosniff`、`allow_network = false`（既定）なら
    同一オリジンと `data:` / `blob:` とインラインのスクリプト・スタイルだけを許す CSP を付ける。
  - ページ内のナビゲーションはベース内に限り、`http(s)` / `mailto` のリンクと `window.open` は既定のブラウザで
    開く（pane はレポートのまま）。CSP でネットワークを止めていても、この経路での送信は防げない。

## UI

- 左: サイドバー（プロジェクトごとのエージェント一覧と接続状態、起動できるエージェント、レポート一覧、
  gateway ワークスペースのポート一覧）。
- 中央: タブバーと pane 木。各 pane はヘッダ（タイトル・状態・分割/閉じるボタン）を持つ。
- `mod+k`: パレット。開いている pane へ移動（フォーカス中のものを除く）、全プロジェクトのエージェントを開く、
  全プロジェクトのレポートと gateway ワークスペースのレポートを開く、レイアウト操作（新規タブ・分割・閉じる・プロジェクトを開く）を 1 つの
  あいまい検索（`ukai_core::palette`。空白区切りの語ごとに部分列一致、語頭と連続一致を優先）で選ぶ。
  ↑↓ で選択、Enter で実行、Esc で閉じる。項目は開いた時点で集める。
  report pane の子 WebView は DOM より手前に出るので、パレットを開いている間は隠す。
- **キーバインド。** ユーザーの `config.toml` の `[keybindings]` で、アクション名 → chord（1 つか配列、`[]` で解除）を
  既定に上書きする（`ukai_core::keymap`）。chord は `mod+shift+d` の形で、`mod` は macOS で cmd、それ以外で ctrl。
  アクションはアプリ全体で効くものと、入力欄（と差分コメントの入力）でだけ効く `send` に分かれる。
  ctrl / alt / cmd なしの chord は、文字を打つキーには使えない（入力欄の文字入力を奪うため）。アプリ全体の
  アクションではさらにファンクションキーと `escape` だけ（enter や tab は入力欄で意味を持つため）。
  `send = "mod+enter"` にすると素の Enter は改行になる。`stop` はパレットを開いている間は効かない（Esc はパレットを閉じる）。
  照合は DOM の `key` に加えて `code`（US 配列での素のキー）でも行い、⌥ や ⇧ で文字が変わっても効く。
  ユーザーが割り当てた chord は、それを既定で持っていたアクションから外す。未知のアクション・解釈できない chord・
  ユーザー設定同士の衝突は全部集めてサイドバー下部に警告として出し、残りの設定は効かせる。起動時に 1 回読む。
  アクション: `palette` `new-tab` `close-pane` `split-right` `split-down` `next-tab` `prev-tab` `next-pane`
  `prev-pane` `tab-1`〜`tab-9` `stop` `dictate`（以上アプリ全体）、`send`（入力欄）。

  ```toml
  [keybindings]
  palette = "mod+p"
  send = "mod+enter"
  next-tab = ["ctrl+tab", "mod+alt+right"]
  close-pane = [] # 解除
  ```
- **クリップボード。** macOS では cut / copy / paste / undo / select all が Edit メニュー経由で WebView に届くので、
  Edit メニューを持つ最小のメニューバーを付ける（Close Window は `mod+w` と衝突するので入れない）。
  他の OS は WebView が自前で処理するのでメニューバーは出さない。
- エージェント pane: メッセージのストリーミング表示、ツール呼び出しの折りたたみ、plan のチェックリスト、
  パーミッション要求はインラインのカードで応答。
- スタイルは素の CSS + CSS 変数（ライト/ダークはOS追従）。Node ツールチェーンを持ち込まないため Tailwind は使わない。
  見た目は orca に倣う: ニュートラルな配色（主色は黒/白、面の差は前景色の数 % で付ける）、細い境界線、
  pane 同士は隙間なしで 1px の区切り線、アイコンはインライン SVG。
- サイドバーはプロジェクトごとに、開いている agent pane を状態ドット付きで並べる（クリックでそのタブと pane に
  フォーカス）。その下に選択中プロジェクトで起動できるエージェント。
- エージェントの状態は全面（サイドバー・タブ・pane ヘッダ）で同じ語彙: 作業中は黄のスピナー、待機は緑、
  **未回答のパーミッション要求がある間はオレンジ**（`Transcript::awaiting_permission`）、エラー切断は赤、それ以外は灰。
  タブは中の pane のうち最も注意を要する状態を出す。
- **未読と通知（orca に倣う）。** ターン終了（ユーザーの cancel を除く）・パーミッション要求・prompt の失敗・
  エラー切断を `ukai_core::Alert` とし、その pane を見ていなければ（ウィンドウが前面で、かつフォーカス中の pane
  でなければ）未読にする。未読はサイドバーの行（太字 + 青い点）・タブ（青い点）・pane ヘッダ（`new`）に出し、
  pane にフォーカスが移るかウィンドウが前面に戻ったときにフォーカス中の pane から消す。ウィンドウが背面にある間は
  デスクトップ通知（`notify-rust`。Linux は zbus なので libdbus は不要）も出し、パーミッション要求ではウィンドウの
  注意喚起もする。通知の本文は `エージェント · プロジェクト` と要約だけで、ヘッダ値など設定由来の値は載せない。
- **差分のレビュー（orca に倣う）。** 差分を持つツール呼び出しは `+n −m` 付きの行になり、開くと行番号付きの
  unified diff（前後 3 行、`similar`。1 呼び出し 400 行まで）を出す。行にホバーして「+」でコメントを付けると
  入力欄の上に溜まり、次の送信で `review_prompt` が「場所・コメント・該当行」の一覧を本文の前に付けて送る
  （本文が空でも送れる）。対象はエージェントが ACP で報告した差分だけで、worktree の `git diff` ではない。
- **添付（orca に倣う）。** 入力欄へのファイルのドロップと 📎（ファイル選択ダイアログ）で、ファイルや画像を
  次の prompt に添える。送り方は前述の `ukai_core::attachment` が決め、断った理由は入力欄に出す。ユーザー発言には
  添付のファイル名を出す。下書き・添付・レビューコメントは pane のセッションを繋ぎ直しても残る。
- **過去のセッション（hermes に倣う）。** エージェントが `sessionCapabilities.list` を持つと、agent pane のヘッダに
  時計のボタンを出す。開くと `session/list`（pane のプロジェクトの cwd で絞り、`nextCursor` を 10 ページまで追う）で
  pane 以外のセッションを更新日時の新しい順に並べ、選ぶとその id を pane に記録して `session/load` / `session/resume`
  でつなぎ直す（「New session」で新規）。ターン中は切り替えない（接続を落とすとターンが切れうるため）。
  relay は `session/list` の cwd もワークスペースのものに書き換える。fork（`session/fork`）は ACP で unstable なので未対応。
- **音声入力。** 入力欄の 🎤（既定 `mod+shift+m`、フォーカス中の pane の入力欄に効く）で録音を始め、もう一度押すと
  止めて文字起こしし、下書きの末尾に足す（日本語・中国語の間は空白なしで、それ以外は空白で繋ぐ）。文字起こしは
  **端末内で完結させる**: whisper.cpp（`ukai-voice`。macOS では Metal）で、外部の API には何も送らない。
  モデル（ggml 形式。`ggml-base.bin` など）はユーザーが置き、ユーザーの `config.toml` で指す。
  ```toml
  [voice]
  model = "models/ggml-base.bin" # 相対パスは config.toml のディレクトリから
  language = "ja" # ISO 639-1。省略で自動判定
  ```
  - モデルは最初の録音の開始時に読み込む（話している間に UI スレッド外で）。文字起こしも `spawn_blocking`。
  - 録音は既定の入力デバイスをモノラルにして持ち、16 kHz に線形リサンプルして渡す。最長 5 分で打ち切る。
    無音（振幅が閾値以下）は送らない（whisper は無音から文章を作りがちなため）。`[BLANK_AUDIO]` などの
    非音声の印は捨てる。
  - 録音中は 🎤 が赤く点滅、文字起こし中は黄。問題（モデルがない、マイクがない、何も聞こえない）は入力欄の
    エラー行に出す。pane を閉じると録音も止まる。
  - macOS で `.app` に包むときは `Info.plist` に `NSMicrophoneUsageDescription` が要る（「配布」で対応）。
  - macOS の SFSpeechRecognizer などの OS の認識器は使わない: Objective-C の `unsafe` 呼び出しが要り
    （ワークスペースは `unsafe_code = "forbid"`）、OS ごとに別実装になり、Linux で試せないため。
- **使用量。** エージェントが ACP の `usage_update` を送ると、入力欄の右下に最新のコンテキスト使用量と累計コスト
  （`53k / 200k (27%) · $0.05`、`ukai_core::usage_label`）を出す。送らないエージェントでは何も出さない。
- **スラッシュコマンド。** 下書きが `/` と名前の途中だけのとき、エージェントが `Commands` で知らせたコマンドを
  あいまい一致（`palette` と同じ照合）で入力欄の上に出す。↑↓ で選び、Tab / Enter で `/name` に置き換え、
  Escape で閉じる。送るのは普通の prompt で、relay は `session/update` として記録するので `session/load` で一覧も戻る。
- **スケジュール。** 後述「スケジュール」の通り gateway が持って回し、アプリは編集と結果の表示だけをする。
  サイドバーの「Schedules」に選択中のプロジェクトの gateway エージェントごとの一覧（15 秒ごとに読み直す）、
  schedule pane（`PaneContent::Schedule { project, agent, schedule }`、未保存は `schedule: None`）で名前・中身・
  cron（プリセットと次回時刻の表示）を下書きとして編集して保存、有効化、「Run now」、停止、削除する。
  run の一覧と出力の末尾も pane に出す（開いている間は 4 秒ごとに読み直す）。失敗はウィンドウが背面なら
  デスクトップ通知。prompt の run のセッションは「Open session in a pane」でそのスケジュールのワークスペース
  （`pod` スコープ、鍵は `workspace_key`）の agent pane に開き、`session/load` で読んで続けたり、
  待っているパーミッション要求に答えたりできる。
- 会話は最大幅 46rem で中央寄せ。ユーザー発言は右寄せの淡いバブル、エージェント発言は地のまま、ツール呼び出しは
  1 行ずつ、思考は折りたたみ。入力欄は角丸カードで、送信/停止は右下の丸ボタン。
- **steer。** ターン中の送信はそのターンへの割り込みになる。ACP 本体には走っているターンに発言を足す手段がない。
  - `initialize` 応答の `_meta.steering.supported` を広告するエージェント（`claude-agent-acp`。Zed もこれを使う）には
    拡張リクエスト `_session/steering`（`ukai_acp::STEER_METHOD`）で発言をターンに注入する（`SessionHandle::steer`、
    広告は `PromptCaps::steering`）。`_meta.steering.idleBehavior = "promptRequired"` を付け、ターンがもう終わって
    いて `promptRequired` が返ったら同じ内容を `session/prompt` で送り直し `TurnRunning` を出す。拒否されたら
    `SteerFailed`。relay はターン中の steer の発言を履歴に記録する（`promptRequired` になったものは続く
    `session/prompt` で記録される）。
  - 広告しないエージェントには `session/cancel` でターンを止め（未回答のパーミッション要求も cancelled で答える）、
    発言は `Transcript::queue` に積んで、ターンの終わり（`TurnEnded` / `PromptFailed`）を待って次の prompt として
    送る。止まるまでに重ねた発言は 1 つにまとめる。待っている発言は会話の末尾に破線のバブルで出し、「Edit」で
    入力欄へ戻せる。送れないまま切断したときも入力欄へ戻す。
- **エージェント発言と思考は Markdown で描く。** `ukai_core::markdown` が `pulldown-cmark`（CommonMark + 表・
  打ち消し線・タスクリスト）で木に変え、地の文の `http(s)://` もリンクにする。アプリはその木を rsx! の要素に写すだけで HTML 文字列は作らない。会話は
  IPC を持つアプリの web view 内にあるため、生の HTML は文字のまま出し、画像は読み込まず alt テキストだけ、
  リンクは `https://`・`http://`・`mailto:` のときだけシステムのブラウザで開く（web view 自体は遷移しない）。
  ユーザー発言は打ったとおりのプレーンテキスト。

## 状態と並行性

- tokio ランタイム上で接続ごとにタスクを立て、`SessionEvent` を `mpsc` で UI 側へ送る。
- UI 側は pane ごとにアプリスコープの `Signal<Transcript>` を持ち、`spawn_forever` のタスクでイベントを畳み込む。
- `ukai-core` の状態遷移は `fn apply(state, event) -> state` 形式の純粋関数にし、ユニットテストとプロパティテストで固める。
- ログは `tracing`。ファイル（`~/.local/state/ukai/ukai.log`）と stderr に出す。

## ブラウザ pane とポート

エージェントが立てた dev server（Web アプリ、ドキュメント、コンポーネントカタログ）をチャットの横で見る。
pane の中身は `browser { project, port, path, source }`。`source` はレポートと同じ `ReportSource` で、`project`
（既定）なら手元のポート、`agent { agent, workspace }` なら gateway エージェントのワークスペースのポート。

### 手元のポート

`http://localhost:<port><path>` をそのまま開く（`::1` だけで listen する dev server もあるので `127.0.0.1` では
なく `localhost`）。サイドバーの「Ports」の「+」か空 pane の「Open a local port…」で `5173` や
`localhost:3000/docs` を打つ（`ukai_core::browser::parse_target`。手元の名前しか受けない）。

### ワークスペースのポート（トンネル）

Pod の中のポートには手元から届かない。HTTP をパスのプレフィックスで中継すると、dev server の絶対パスや
HMR の WebSocket が壊れるので、kubectl の port-forward と同じく **TCP をそのまま運ぶ**:

```
browser pane ──▶ 127.0.0.1:<port>（ukai_acp::ports::Tunnel）
                   │ 受けた TCP 接続 1 本ごとに WebSocket 1 本（binary フレーム）
                   ▼
   ukai-gateway  /agents/<id>/forward/<port>   （ワークスペースとトークンは /acp と同じ）
                   ▼
   ukai-relay    /forward/<port>  ──TCP──▶ Pod 内の localhost:<port>
```

- トンネルは pane が表示している間だけ開き、`127.0.0.1` だけで listen する。ポートは Pod 側と同じ番号が空いて
  いればそれ（オリジンが変わらないので、ストレージや OAuth のリダイレクト先が保たれる）、なければ任意。
  `SO_REUSEADDR` は付けない（macOS では `*:<port>` の既存サーバーの横に bind でき、`127.0.0.1` への接続を
  奪ってしまうため）。web view も `127.0.0.1` を開く（`localhost` だと `::1` の別のサーバーに当たりうる）。
- WebSocket の upgrade は reqwest で行う（エージェントのヘッダと TLS 設定をそのまま使う。IAP の背後でも通る）。
  gateway は中継中クライアントに 20 秒ごとに Ping を送るので、静かな接続もプロキシに切られない。
- relay は Pod の中から `localhost` につなぐので、`127.0.0.1` だけで listen する dev server にも届く。
- 届かないとき（Pod が止まっている 410、何も listen していない 502）は、トンネルがその接続に理由を書いた
  HTTP 応答を返す（browser pane にただのリセットを見せないため）。

### 一覧とプレビュー

- relay の `/ports` は `/proc/net/tcp*` の LISTEN を読み（Linux だけ）、`/proc/<pid>/fd` からプロセス名を添える。
  relay 自身のポートは除く。
- エージェントは Pod のポートが見えないことも、ukai が開けることも知らない。そこで relay の MCP サーバーの
  `instructions` に伝え、`open_preview(port, path, title)` ツールを出す。記録は relay の一時ディレクトリの
  JSON Lines ファイル（relay の起動時に空にする）で、`/ports` が最新 20 件を返す。`path` はオリジンに足すだけの
  パスに限る（`ukai_acp::ports::preview_path`）。
- アプリ（`ukai-app/src/ports.rs`）は開いている gateway エージェント pane のワークスペースの一覧を 3 秒ごとに
  読み、サイドバーの「Ports」に出す（クリックで browser pane）。前回より新しいプレビューは、そのポートの
  browser pane があればそこで、なければエージェントの pane の右に開く。フォーカスは動かさない。初めて読んだ
  ワークスペースのプレビューは開かない（起動し直すたびに全部開かないため。`ukai_core::browser::new_previews`）。

### 描画

レポートと同じく **IPC ハンドラを持たない子 WebView**（`ukai-app/src/child_view.rs` を共有）で、`incognito`。
違うのは、ページが信頼できないことは同じでも行き先を制限しないところ: `http(s)` へのナビゲーションは pane の中で
進み、それ以外のスキームと新しいウィンドウは既定のブラウザに渡す。CSP も付けない（応答はアプリが作らない）。
pane の上部に戻る・進む・再読込・アドレス（オリジン上のパス。Enter で移動）・外部ブラウザで開くを置く。
子 WebView はタブの切り替えで作り直すので、ページの状態（ログインなど）はそこで失われる。

## Security

- `fs/*` はプロジェクトルート配下に正規化後のパスで制限。シンボリックリンク経由の脱出も拒否。
- 認証情報は環境変数かキーチェーンの参照のみ。設定ファイル・state ファイル・ログに値を書かない（ヘッダ値はログでマスク）。
- HTTP エージェントは `https://` / `wss://` を既定とし、平文は `localhost` 以外で警告。
- HTML レポートは前述の IPC を持たない子 WebView + 配信範囲の制限 + CSP。
- browser pane も IPC を持たない子 WebView。トンネルは `127.0.0.1` だけで listen し、pane を閉じると止まる
  （開いている間は手元の他のプロセスからも使える。kubectl の port-forward と同じ）。relay の `/forward` は
  トークンが要り、Pod の中の `localhost` にしかつながない。

## Milestones

| Phase | 内容                                                                                                                                                                                                                        |
| ----- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0     | 環境構築: workspace、mise、CI、lint、`ukai.toml` のスキーマ・検証・`${env:}` 展開、spec → トランスポート解決、レイアウトを読み取り専用で描く Dioxus シェル                                                                  |
| 1     | stdio エージェントとのチャット（agent pane ごとに 1 セッション）、`fs/*`、パーミッション応答、`ukai-core::Transcript`。claude-agent-acp / codex-acp で確認済み                                                              |
| 2     | HTTP / WebSocket 接続、`ukai-relay` と `ukai-gateway`（Kubernetes 上のワークスペース Pod、切断をまたぐセッション）、複数エージェント                                                                                        |
| 3     | 実行時のタブと pane、プロジェクト（複数・pane ごと）、配置とセッション id の保存・復元、パレット、未読と通知、差分レビュー、添付、スラッシュコマンド、音声入力、gateway のスケジュール（コマンドと prompt、済み）。リサイズ |
| 4     | HTML / 画像の report pane（IPC のない子 WebView、済み）、更新への追従（済み）、browser pane と gateway ワークスペースのポートへのトンネル・`open_preview`（済み）、ACP コンテンツからのレポート                             |
| 5     | パーミッション規則、再接続・`session/load`、配布（`dx bundle`）                                                                                                                                                             |

## Open questions

- ナレッジを手元の stdio エージェントにも渡すか（今は gateway の Pod だけ）。渡すなら置き場所が要る。
- `open_preview` を手元の stdio エージェントにも渡すか（relay の MCP サーバーを手元でも足すことになる）。
  手元のポートの一覧も今はない（macOS には `/proc` がなく、マシン全体の listen は雑音が多い）。
- browser pane の子 WebView を保存領域ありにするか（タブを切り替えてもログインが残る）。macOS では既定の
  データストアがアプリの web view と共有になるので、分けるなら `data_store_identifier`（macOS 14+）が要る。
- 認証: RFD は認証を別ページに切り出している。OAuth 系のクラウドエージェントをどう扱うか（ブラウザで
  ログインしてトークンを受け取るフロー）を Phase 2 で決める。
- ACP v2（`unstable_protocol_v2`）をいつ追うか。`SessionEvent` 境界で吸収できる前提だが要確認。
- 同一エージェントへの複数セッションを 1 connection に載せるか、セッションごとに connection を張るか
  （サーバ側実装の対応状況次第）。
- `_ukai/turn_state` / `_ukai/progress` を、ACP v2 の `state_update` やリモートトランスポートの仕様が
  固まったらそちらに置き換える。
- snapshot の `refs/ukai/wip/*` を各ホスティングが受けるか。ブランチにすると CI の push トリガーに乗る。
- レポートのコピーを Pod が落ちていなくても見せるか（relay が遅いときなど）、版を残すか（今は最新だけ）。
- gateway の外部公開と認証。port-forward のほか、認証するリバースプロキシ（Ingress の前の IAP など、
  bearer token を検証するもの）越しに `wss://` で使えることを確認済み。その場合 `Authorization` は
  プロキシのトークンに使うので、gateway 自身の `UKAI_GATEWAY_TOKEN` は設定しない。トークンには期限があり、
  期限切れは接続エラーとして出るだけで、自動更新はまだない。
