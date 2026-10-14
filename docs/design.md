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
  (Goose server, ukai-gateway      (claude-agent-acp, codex-acp,
   + ukai-relay per stdio agent)    gemini --experimental-acp, ...)
```

### Crates

| crate          | kind      | 役割                                                                                                                                                   |
| -------------- | --------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ukai-config`  | lib       | `ukai.toml` のスキーマ（serde 型）、読み込み、変数展開、検証。I/O 以外は純粋関数で、テストの主戦場。                                                   |
| `ukai-acp`     | lib       | エージェント接続。`AgentSpec` → トランスポート選択 → ACP client 接続、セッション操作、`SessionEvent` への正規化。UI に依存しない。                     |
| `ukai-core`    | lib       | アプリ状態のモデル（実行時のタブ/pane 木 `Layout`、会話ログ `Transcript`。今後レポート索引）。UI フレームワークに依存しない。                          |
| `ukai-app`     | bin       | Dioxus desktop UI。`ukai` バイナリ。状態は `ukai-core` を Signal で包むだけに留める。                                                                  |
| `ukai-relay`   | bin + lib | stdio エージェントを子に持ち、`/acp`（HTTP/SSE・WebSocket）で出す。セッションと履歴を接続より長く保ち、再接続に `session/load` の再生で答える。        |
| `ukai-gateway` | bin       | Kubernetes の namespace に 1 つ。`gateway.toml` のエージェントを `/agents/<id>/acp` で出し、ワークスペースごとの Pod を用意して中の relay に中継する。 |

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
`Preparing` / `Ready` / `AgentMessage` / `AgentThought` / `ToolCall` / `ToolCallUpdate` / `Plan` /
`PermissionRequested` / `FileWritten` / `TurnRunning` / `TurnEnded` / `PromptFailed` / `Closed`。
`Preparing` と `TurnRunning` は relay / gateway の拡張通知（`_ukai/progress`、`_ukai/turn_state`）から作る。
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
  `_ukai/` の通知（`progress` / `heartbeat`）が届くたびに数え直すので、Pod の起動を待つ間は切れない。

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
- エージェントには `initialize` を 1 回だけ送り、各クライアントの `initialize` には記録で答える
  （`loadSession` を広告する）。
- セッションごとに `session/update` と、転送したユーザーの prompt を記録する。既知のセッションへの
  `session/load` には記録の再生で答えるので、切断中に起きたことも履歴に入る。relay が知らないセッション
  （relay の再起動後など）はエージェントに load させ、その再生を記録する。
- ターンはクライアントがいなくても続く。実行中に再接続したクライアントには `_ukai/turn_state`
  （ACP v2 の `state_update` に合わせた形: `running` / `idle` と `stopReason`）で伝える。
- エージェントからの要求（パーミッション）はクライアントが来るまで保留し、つないできたクライアントに出し直す。
- クライアントの `cwd` はクライアントの手元のパスなので、`session/new` / `load` / `resume` の `cwd` は
  relay の作業ディレクトリに置き換える。エージェントには `fs/*` も terminal も広告しない。
- 1 セッションに付くクライアントは 1 つで、後から load したほうが引き継ぐ。
- クライアントがおらずターンも動いていない状態が `idle_timeout` 続いたら終了する。

### `ukai-gateway`

- namespace に 1 つ（StatefulSet）。`gateway.toml` の `[agents.<id>]`（イメージ、コマンド、リポジトリ、
  Secret 参照、既存 PVC の mount、リソース、`idle_timeout`）を `GET /agents/<id>/acp` の WebSocket で出す。
- 接続が来たらそのワークスペースの Job を確保して Pod の ready を待ち、Pod 内の relay に中継する。
  待つ間はクライアントに `_ukai/progress`（状況が変わるたび）と `_ukai/heartbeat`（20 秒ごと）を送り、
  クライアントからのメッセージは溜めておく。
- 権限は namespace の Role だけ（jobs、pods と events の参照）。CRD も cluster-scoped な権限も要らない。
- pane ごとのワークスペース: ukai は gateway のエージェントを開いた pane に鍵を作って `state.json` に残し、
  接続のたびにヘッダー `Ukai-Workspace-Scope`（`shared` / `pod`）と `Ukai-Workspace`（鍵）を送る。
  `shared` はそのエージェントの全 pane で 1 つの Pod と作業ツリーを共有し、`pod` は pane ごとに Pod を分ける
  （鍵がワークスペースの識別子に入る）。既定は設定の `workspace` で、pane を開くときに選び直せる。
  同じ Pod の中で作業を分けたいときは、エージェント自身に git worktree を使わせる。
- セッションの状態は relay が持つので、gateway を再起動してもエージェントは落ちない。

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

`deploy/gateway` が kustomize の base、`images/runtime` が gateway と relay のイメージ、`images/dev` が
このリポジトリを開発するエージェント用のイメージ（`mise.toml` のツール群、desktop のビルド依存、
仮想ディスプレイ入り）。

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

[agents.cluster-claude] # ukai-gateway のエージェント（「クラウド実行」）
transport = "gateway"
url = "wss://ukai.example.com/agents/claude/acp"
headers = { Authorization = "Bearer ${env:UKAI_GATEWAY_TOKEN}" }
workspace = "pod" # 新しい pane の既定: shared | pod

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
  ukai ではリモートエージェントは元々サーバー側で生きている。ローカルの stdio エージェントも `ukai-relay` を
  ローカル常駐させて WebSocket で話す形にすれば同じ扱いにできる。

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
  見た目は orca に倣う: ニュートラルな配色（主色は黒/白、面の差は前景色の数 % で付ける）、細い境界線、
  pane 同士は隙間なしで 1px の区切り線、アイコンはインライン SVG。
- サイドバーはプロジェクトごとに、開いている agent pane を状態ドット付きで並べる（クリックでそのタブと pane に
  フォーカス）。その下に選択中プロジェクトで起動できるエージェント。
- エージェントの状態は全面（サイドバー・タブ・pane ヘッダ）で同じ語彙: 作業中は黄のスピナー、待機は緑、
  **未回答のパーミッション要求がある間はオレンジ**（`Transcript::awaiting_permission`）、エラー切断は赤、それ以外は灰。
  タブは中の pane のうち最も注意を要する状態を出す。
- 会話は最大幅 46rem で中央寄せ。ユーザー発言は右寄せの淡いバブル、エージェント発言は地のまま、ツール呼び出しは
  1 行ずつ、思考は折りたたみ。入力欄は角丸カードで、送信/停止は右下の丸ボタン。

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
| 2     | HTTP / WebSocket 接続、`ukai-relay` と `ukai-gateway`（Kubernetes 上のワークスペース Pod、切断をまたぐセッション）、複数エージェント                           |
| 3     | 実行時のタブと pane、プロジェクト（複数・pane ごと）、配置とセッション id の保存・復元（済み）。リサイズ、パレット                                             |
| 4     | HTML レポート pane（spike → sandbox iframe or 別ウィンドウ）、ファイル監視、ACP コンテンツからのレポート                                                       |
| 5     | パーミッション規則、再接続・`session/load`、配布（`dx bundle`）                                                                                                |

## Open questions

- 認証: RFD は認証を別ページに切り出している。OAuth 系のクラウドエージェントをどう扱うか（ブラウザで
  ログインしてトークンを受け取るフロー）を Phase 2 で決める。
- ACP v2（`unstable_protocol_v2`）をいつ追うか。`SessionEvent` 境界で吸収できる前提だが要確認。
- 同一エージェントへの複数セッションを 1 connection に載せるか、セッションごとに connection を張るか
  （サーバ側実装の対応状況次第）。
- `_ukai/turn_state` / `_ukai/progress` を、ACP v2 の `state_update` やリモートトランスポートの仕様が
  固まったらそちらに置き換える。
- snapshot の `refs/ukai/wip/*` を各ホスティングが受けるか。ブランチにすると CI の push トリガーに乗る。
- gateway の外部公開と認証（今は bearer token か port-forward）。
