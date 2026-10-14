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
- **HTML レポートの描画。** エージェントが生成したレポートを HTML のまま pane に表示し、
  ファイル更新に追従して再描画する。
- Rust 単一ワークスペース。UI は [Dioxus](https://dioxuslabs.com) 0.7 の desktop renderer。

## Non-goals (初期)

- エディタ機能（コード編集・LSP）。ukai はエージェントの操縦席であって IDE ではない。
- 端末エミュレータ pane。将来の pane 種別として予約だけしておく（後述）。
- ACP エージェント側の実装。ただし stdio エージェントを HTTP に公開する薄いブリッジは持つ（`ukai-bridge`）。
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
│  │ reports  │ │ (chat, tools │(sandboxed │ │                               │   │
│  │          │ │  permissions)│ iframe)   │ │                               │   │
│  │          │ └──────────────┴───────────┘ │                               │   │
│  └──────────┴──────────────────────────────┴───────────────────────────────┘   │
│        ▲ Signals (UiState)                    ▲ ukai://report/... (asset handler)│
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
  (Goose server, ukai-bridge       (claude-agent-acp, codex-acp,
   wrapping any stdio agent, ...)   gemini --experimental-acp, ...)
```

### Crates

| crate         | kind | 役割                                                                                                                                                     |
| ------------- | ---- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ukai-config` | lib  | `ukai.toml` のスキーマ（serde 型）、読み込み、変数展開、検証。I/O 以外は純粋関数で、テストの主戦場。                                                     |
| `ukai-acp`    | lib  | エージェント接続。`AgentSpec` → トランスポート選択 → ACP client 接続、セッション操作、`SessionEvent` への正規化。UI に依存しない。                       |
| `ukai-core`   | lib  | アプリ状態のモデル（実行時のタブ/pane 木 `Layout`、会話ログ `Transcript`。今後レポート索引）。UI フレームワークに依存しない。                            |
| `ukai-app`    | bin  | Dioxus desktop UI。`ukai` バイナリ。状態は `ukai-core` を Signal で包むだけに留める。                                                                    |
| `ukai-bridge` | bin  | (Phase 2) 任意の stdio ACP エージェントを `AcpHttpServer` で `/acp` に公開する。これでローカル用エージェントもコンテナ / VM に置いて HTTP 越しに使える。 |

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
  クラウドエージェントがこれを使えば、レポートを手元に直接書かせられる（HTML レポートの主経路）。
- `terminal/*` … 初期は capability を広告しない。

### セッションのライフサイクル

1. `connect(agent_id)` → `initialize`（capability 交換、認証が必要なら `authenticate`）
2. `session/new { cwd, mcpServers }` → `SessionHandle`
3. `session/prompt` を投げ、`session/update` 通知を `SessionEvent` に変換して UI へ流す
4. `session/cancel`、切断時は `session/load`（agent が `loadSession` を広告している場合）で復帰

接続 1 本に複数セッションを載せられる（RFD の connection/session 2 層構造に合わせる）。
`SessionRegistry` が `(agent_id, session_id)` をキーにハンドルを保持する。

### イベント正規化

UI は ACP のスキーマ型を直接触らない。`ukai-acp` が `SessionEvent`（`crates/ukai-acp/src/session.rs`）に畳む:
`Ready` / `AgentMessage` / `AgentThought` / `ToolCall` / `ToolCallUpdate` / `Plan` /
`PermissionRequested` / `FileWritten` / `TurnEnded` / `PromptFailed` / `Closed`。
UI からは `SessionHandle`（`prompt` / `cancel` / `resolve_permission`）で操作する。
ACP のスキーマ変更（v2 への移行など）の影響をこの境界で吸収する。

実装上の注意（SDK `agent-client-protocol` 3.3）:

- エージェントからのメッセージは接続レベルのコールバックで「変換して転送するだけ」にする。
  コールバックは SDK のディスパッチループを占有するので、ユーザー待ちをそこでしてはいけない。
  パーミッション要求は responder をコマンドループに預け、ユーザーの回答で応答する。
- `ActiveSession::send_prompt` は prompt がエラーになるとコールバックのエラーで接続ごと落ちる。
  prompt は `prepare_request(...).on_receiving_result` で自前で送り、エラーは `PromptFailed` に変換する
  （`prepare_request` にすることで、ターン終了がそれ以前の `session/update` より後に届く順序も保証される）。
- `initialize` + `session/new` には 120 秒のタイムアウトを付ける（認証切れで無応答になるエージェント対策）。

### 再接続

RFD v1 はメッセージ再送を定義していない（v2 で `Last-Event-ID` 等）。
初期実装は「切断を UI に明示 → 手動 / 指数バックオフで再接続 → `session/load`」まで。
途中のターンは失われうることを UI 上で正直に出す。

## プロジェクト設定 `ukai.toml`

### 探索と合成

1. ユーザー全体: `~/.config/ukai/config.toml`（`$UKAI_CONFIG_DIR` / `$XDG_CONFIG_HOME/ukai` で変更可）
   … 共有のエージェント定義（どのプロジェクトでも使うクラウドエージェント）
2. プロジェクト: プロジェクトルートの `ukai.toml`（任意）… プロジェクト固有のエージェント
3. （今後）ローカル上書き: `ukai.local.toml`（gitignore 前提）… 個人の URL やトークン参照

`ukai_config::load_layered` が後勝ちで合成する: `agents.<id>` は id 単位で置換、`permissions` は
プロジェクトの規則を先に評価、`project` / `reports` はプロジェクトのもの。検証は合成後に 1 回だけ行う
（ユーザー側で定義したエージェントをプロジェクトの規則から参照できるように）。
**秘密情報はファイルに書かせない**。`${env:NAME}` 展開のみ許し、未定義なら起動時にエラーではなく
そのエージェントを「未構成」として表示する。

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

# ---- パーミッション自動応答（未実装） -------------------------------------
[[permissions]]
agent = "claude"
tool_kind = "read" # ACP ToolKind
decision = "allow"

# ---- レポート ------------------------------------------------------------
[reports]
dirs = ["reports"] # 監視して一覧に出すディレクトリ（プロジェクトルート相対）
globs = ["**/*.html"]
```

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
  ショートカットは `mod+t` 新規タブ、`mod+d` 右に分割、`mod+shift+d` 下に分割、`mod+w` pane を閉じる
  （`mod` は macOS で cmd、それ以外で ctrl）。
- セッション・会話ログ・入力中の下書きは pane id をキーにアプリスコープで保持する。分割やタブ切り替えで
  コンポーネントが作り直されてもセッションは切れない。pane を閉じるとセッションも終わる。
- 今後: 境界のドラッグでのリサイズ（比率はモデルにある）、パレット。

## プロジェクトと永続化

herdr（JSON 1 ファイル）と orca（SQLite のドキュメントストア + 会話ジャーナル DB）を参考にした。

- **プロジェクト** = ルートディレクトリ + 不透明な id（`p1`, `p2`, …、再利用しない）。pane は id で参照する。
  パスを id にしないのは、後で worktree やリモートの作業ディレクトリを同じプロジェクトに束ねるため。
  サイドバーでフォルダ選択ダイアログから追加、`ukai <dir>` でも追加（引数なしなら cwd から `ukai.toml` を
  遡って探す）。閉じるとそのプロジェクトの pane とセッションも閉じる。
- **DB は使わない。** 永続化するのはプロジェクトの登録とタブ/pane の配置（各 agent pane の ACP セッション id を含む）
  だけで、`~/.local/state/ukai/state.json`（`$UKAI_STATE_DIR` / `$XDG_STATE_HOME/ukai`）の 1 ドキュメント。
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
  ukai ではリモートエージェントは元々サーバー側で生きている。ローカルの stdio エージェントは `ukai-bridge` を
  ローカル常駐させて HTTP で話す形にすれば同じ扱いにできる。

## HTML レポート

「レポートは HTML で見たい」がこのアプリの主要ユースケースの1つ。

### レポートの出どころ (`ReportSource`)

1. **ファイル** … `reports.dirs` 配下の HTML。`notify` で監視し、更新されたら該当 pane を再読込。
   クラウドエージェントでも、ukai が提供する `fs/write_text_file` 経由で書かせればここに落ちる。
2. **ACP コンテンツ** … `session/update` 内の embedded resource / resource link で
   `mimeType = "text/html"` のもの。メモリ上に保持し、エージェント pane に「Open report」チップを出す。
3. **ツール呼び出しの diff** … `.html` へ書き込む diff を検知したら、適用後のファイルを (1) として扱う。

### 描画方式

- レポートは**信頼しない**入力として扱う（エージェントが生成した任意の HTML/JS）。
- ukai-app が Dioxus desktop のアセットハンドラ（wry の custom protocol）で
  `ukai://report/<id>/…` を配信し、pane 内の `<iframe sandbox="allow-scripts">` で表示する。
  - `allow-same-origin` を付けない → 不透明オリジンになり、親ドキュメントや IPC に触れない。
  - 相対パスの CSS/画像はレポートのディレクトリ内に限って配信（パストラバーサル拒否）。
  - レスポンスに CSP を付与し、外部ネットワークへの fetch は既定で禁止（設定で緩和可）。
- **Spike 必須:** wry の IPC (`window.ipc` / `webkit.messageHandlers`) がサブフレームから
  到達可能かをプラットフォームごとに確認する。到達可能なら、レポートは IPC を持たない別ウィンドウ
  （素の wry WebView）で開く方式に切り替える。

## UI

- 左: サイドバー（エージェント一覧と接続状態、セッション一覧、レポート一覧）。折りたたみ可能。
- 中央: タブバーと pane 木。各 pane はヘッダ（タイトル・状態・分割/閉じるボタン）を持つ。
- `mod+k`（今後）: パレット（エージェントを開く、レポートを開く）。
- エージェント pane: メッセージのストリーミング表示、ツール呼び出しの折りたたみ、plan のチェックリスト、
  パーミッション要求はインラインのカードで応答。
- スタイルは素の CSS + CSS 変数（ライト/ダークはOS追従）。Node ツールチェーンを持ち込まないため Tailwind は使わない。

## 状態と並行性

- tokio ランタイム上で接続ごとにタスクを立て、`SessionEvent` を `mpsc` で UI 側へ送る。
- UI 側は pane ごとにアプリスコープの `Signal<Transcript>` を持ち、`spawn_forever` のタスクでイベントを畳み込む。
- `ukai-core` の状態遷移は `fn apply(state, event) -> state` 形式の純粋関数にし、ユニットテストとプロパティテストで固める。
- ログは `tracing`。ファイル（`~/.local/state/ukai/ukai.log`）と stderr に出す。

## Security

- `fs/*` はプロジェクトルート配下に正規化後のパスで制限。シンボリックリンク経由の脱出も拒否。
- 認証情報は環境変数参照のみ。設定ファイル・state ファイル・ログに値を書かない（ヘッダ値はログでマスク）。
- HTTP エージェントは `https://` / `wss://` を既定とし、平文は `localhost` 以外で警告。
- HTML レポートは前述の sandbox + CSP。

## Milestones

| Phase | 内容                                                                                                                                                           |
| ----- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0     | 環境構築: workspace、mise、CI、lint、`ukai.toml` のスキーマ・検証・`${env:}` 展開、spec → トランスポート解決、レイアウトを読み取り専用で描く Dioxus シェル     |
| 1     | stdio エージェントとのチャット（agent pane ごとに 1 セッション）、`fs/*`、パーミッション応答、`ukai-core::Transcript`。claude-agent-acp / codex-acp で確認済み |
| 2     | HTTP / WebSocket 接続、`ukai-bridge`、複数エージェント                                                                                                         |
| 3     | 実行時のタブと pane、プロジェクト（複数・pane ごと）、配置とセッション id の保存・復元（済み）。リサイズ、パレット                                             |
| 4     | HTML レポート pane（spike → sandbox iframe or 別ウィンドウ）、ファイル監視、ACP コンテンツからのレポート                                                       |
| 5     | パーミッション規則、再接続・`session/load`、配布（`dx bundle`）                                                                                                |

## Open questions

- 認証: RFD は認証を別ページに切り出している。OAuth 系のクラウドエージェントをどう扱うか（ブラウザで
  ログインしてトークンを受け取るフロー）を Phase 2 で決める。
- ACP v2（`unstable_protocol_v2`）をいつ追うか。`SessionEvent` 境界で吸収できる前提だが要確認。
- 同一エージェントへの複数セッションを 1 connection に載せるか、セッションごとに connection を張るか
  （サーバ側実装の対応状況次第）。
