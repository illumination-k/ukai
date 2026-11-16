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

| crate            | kind      | 役割                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| ---------------- | --------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ukai-config`    | lib       | `ukai.toml` のスキーマ（serde 型）、読み込み、変数展開、検証。I/O 以外は純粋関数で、テストの主戦場。                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `ukai-acp`       | lib       | エージェント接続。`AgentSpec` → トランスポート選択 → ACP client 接続、セッション操作、`SessionEvent` への正規化。gateway エージェントのレポート・ポート（とそこへのトンネル `ports`）・スケジュール・ナレッジのクライアント。エージェントの checkout のファイル一覧・コマンド実行・差分（`checkout`）とそのレビュー（`review`）。UI に依存しない。                                                                                                                                                                                                                      |
| `ukai-core`      | lib       | アプリ状態のモデル（実行時のタブ/pane 木 `Layout`、会話ログ `Transcript`、レポートの列挙と配信 `report`、発言の Markdown `markdown`、差分とレビュー `review`、添付 `attachment`、ブラウザ pane のポート指定とプレビュー `browser`、エージェント設定の表示 `config`、パレットの照合 `palette`、スラッシュコマンドの補完 `slash`、送った発言の呼び戻し `recall`、`@` のファイル補完 `mention`、`!` のコマンドの送り方 `shell`、`git diff` の読み取り `diff`、エージェントのレビューの規則 `annotation`、サイドバーの木と開閉 `sidebar`）。UI フレームワークに依存しない。 |
| `ukai-voice`     | lib       | 音声入力。マイクの録音（`cpal`）と whisper.cpp（`whisper-rs`）による端末内の文字起こし。変換（ダウンミックス・リサンプル・下書きへの差し込み）は純粋関数の `audio` に置く。                                                                                                                                                                                                                                                                                                                                                                                             |
| `ukai-app`       | bin       | Dioxus desktop UI。`ukai` バイナリ。状態は `ukai-core` を Signal で包むだけに留める。                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `ukai-relay`     | bin + lib | stdio エージェントを子に持ち、`/acp`（HTTP/SSE・WebSocket）で出す。セッションと履歴を接続より長く保ち、再接続に `session/load` の再生で答える。                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `ukai-gateway`   | bin       | Kubernetes の namespace に 1 つ。`gateway.toml` のエージェントを `/agents/<id>/acp` で出し、ワークスペースごとの Pod を用意して中の relay に中継する。                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `ukai-knowledge` | bin       | ナレッジのノートを SurrealDB に持ち、gateway にだけ gRPC（`ukai.knowledge.v1.KnowledgeStore`）で答える。                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `ukai-proto`     | lib       | 部品の間の gRPC の `.proto` と、そこから生成した message・client・server だけ。`protox` でコンパイルするのでビルドに `protoc` は要らない。ukai の型との変換は型の側（`ukai_acp::knowledge::wire`）に置く。                                                                                                                                                                                                                                                                                                                                                              |

UI 依存を `ukai-app` に閉じ込めるのは、ロジックを headless にテストするため（Dioxus のコンポーネントテストは重い）。

コンポーネントそのものは、ウィンドウなしで動かして確かめる（`ukai-app` の `testing`）。`VirtualDom` の
変更をメモリ上の DOM に書かせ、そこへマウスとキーのイベントを送る。`App` はウィンドウに触る部分
（状態の用意、ウィンドウのイベント）だけを持ち、画面は `Shell` が描くので、テストは `Shell` を
そのまま載せる。テストはプロパティテストで、期待値は実装を写したものにしない。

- アプリ全体（`app/tests.rs`）: ランダムな操作列（pane 領域のクリック・ドラッグ、サイドバー、空の pane の
  ピッカー、コマンドパレット、ショートカット）とエージェント側の出来事（セッションの状態、未読）のあと、
  アプリの状態がモデルと一致し、画面の各部（タブ、pane の見出し、サイドバー、パレット）がその状態を
  表し、差分描画の結果が同じ状態からの初回描画と同じであること。レイアウトは `ukai-core::Layout` に
  同じ操作をしたもの、それ以外はユーザーの言葉で書いたモデル。プロジェクトは存在しないパスに置くので、
  操作で始まったセッションは即座に失敗し、以降はテストがエージェント役をする。
- エージェント pane（`agent_pane/tests.rs`）: 任意のセッションイベント列に対して、トランスクリプトの各
  エントリがその種類・内容・順序で表示され、その時点のトランスクリプトで開いた pane と同じに見えること。
  イベント列がどんなトランスクリプトになるかは `ukai-core` の責任で、ここでは与件。
- メッセージの Markdown（`markdown/tests.rs`）: 既知の構造から書き出した文書が、その構造どおりに
  見えること（期待値は Markdown の意味とアプリの約束から書く）。任意の入力で、生きた要素や属性が
  出ず、テキストが欠けず、ストリーミングで届いても一括で届いたのと同じに見えること。

生成器が効いているかは、ハンドラや描画にわざとバグを入れて落ちることで確かめる。操作を足したら
そのバグも試す。一つずつ引いた操作ではめったに届かない部分（パレットの中、複数タブ）は、目的のある
短い操作列として生成する。

ここで確かめられないもの: 要素の位置の計測（`onmounted`。境界のドラッグは計測結果を与えて始める）、
ネイティブの子 web view、システムのダイアログ、実際に動くセッション（送信、`@`、`!`、音声）、
ウィンドウのフォーカス。コンポーザ、レポート・ブラウザ・diff・スケジュール・ノートの各 pane も対象外。

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
なく `localhost`）。サイドバーの手元の「Ports」の「+」か空 pane の「Open a local port…」で `5173` や
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
- アプリ（`ukai-app/src/ports.rs`）は、レポートと同じワークスペースの一覧を 3 秒ごとに読み、サイドバーの
  そのワークスペースの「Ports」に出す（クリックで browser pane）。pane のあるワークスペースで前回より新しいプレビューは、そのポートの
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
- `ukai-knowledge` は gateway が名乗らせた呼び手を信じるので、gateway と共有するトークンと NetworkPolicy で
  gateway 以外を入れない。
- エージェントの Pod も（自分のナレッジのために）gateway に届く。Pod は gateway のトークンを持たないが、
  トークンのない gateway（IAP の背後で `Authorization` が IAP のもの）では人の要求と区別できないので、
  gateway は送り元がエージェントの Pod（`app.kubernetes.io/managed-by=ukai` の Pod の IP）なら、
  `WorkspaceKnowledge` と `/health` のほかはトークンがあっても 403 で断る（`access::guard`）。Pod の IP は
  1 秒より古ければ引き直す。Pod の IP はエージェントが動く前（relay のコピーと clone の間）からあるので、
  その鮮度で足りる。一覧が引けなければ断る。
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
