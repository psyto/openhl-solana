# 第14章 — Cranks、Keepers、オフチェーン グルー

> 状態: ドラフト (v0.1)。
> 教材コード: なし。本章にオンチェーン追加はゼロ。内容は、13 章かけて組み立てたプログラムを取り囲むオフチェーン運用設計だ。

---

## §14.0  はじめに

Solana DEX プログラムは Solana DEX の半分だ。残りの半分は、オンチェーン プリミティブを正しい cadence で駆動し、結果の状態をユーザに浮かび上がらせる、調整されたオフチェーン プロセス群 — keeper、crank、indexer、監視 — だ。それなしには、美しく監査されたオンチェーン プログラムはユーザがトランザクションを起動するときに 1 回走り、その後アイドルに座る。ファンディングは蓄積しない。水没ポジションは清算されない。NAV は古びる。フロントエンドは数分前の最終既知状態を表示する。

本章では運用層を歩く。本プログラムが暗黙に要求するすべての keeper と crank を、それぞれ実装できるだけの詳細で。本章は意図的にオンチェーン コードが少なく（追加しない）、本番設計パターン — 失敗モード、冗長性、手数料経済、cadence 選択 — が長い。これらを怒りの中で走らせて初めて学べるパターンだ。

Keeper 6 つ、indexer 1 つ:

1. **ファンディング レート keeper** — 各 market で定期的に `UpdateFunding` を呼ぶ。レートを book mid + オラクル マークから計算する。
2. **清算 bot** — ポジションをスキャンし、水没ポジションを特定し、無許可 `Liquidate` 呼び出しを提出する。
3. **Vault NAV reporter** — 各 vault について、マネージャ（あるいはその代理）が定期的に `UpdateNAV` を呼ぶ。
4. **Builder claim cron** — 各 builder が定期的に `ClaimBuilderFees` で `accumulated_fees` を排出する。
5. **オラクル publisher** — Pyth ではなく自前オラクルを走らせるなら、これが `SetOraclePrice` で新鮮な価格を push するプロセス。
6. **メンテナンス keeper** — 細々と: 休眠アカウントの close、約定済み注文のアーカイブ、空板スロットのガベージ コレクト。
7. **Indexer** — keeper ではないが必須の読み手側サービス: チェーン状態を購読し、フロントエンド、分析、アラートに供給する。

本章は Phase B とトラックを閉じる。§14.7 の振り返り後はチャプターは存在しない。第 0 章でカリキュラムがコミットした意味でプログラムを feature-complete にする前に追加するオンチェーン機能はない。その後どこに行くかはあなた自身の選択だ — 本番デプロイ、監査準備、スケーリング実験、独自 perp DEX。ピースはあなたの手にある。

---

## §14.1  Keeper インベントリ

| Keeper | トリガ | Cadence | 認可 | ないと壊れること |
|---|---|---|---|---|
| ファンディング レート keeper | Market ごと | 1〜5 分 | 許可制 or クランプ付き無許可 | ファンディング蓄積が止まる。ロング/ショートが互いに支払わなくなる。マークが spot から漂流する |
| 清算 bot | ポジションごと | Slot ごと（sub-second スキャン） | 無許可 | 水没ポジションがオープンのまま。保険基金が最終的に枯渇する |
| Vault NAV reporter | Vault ごと | 1 分〜1 時間 | Vault マネージャのみ | Vault NAV が古びる。deposit/withdraw が誤った NAV で値付けされる |
| Builder claim cron | Builder ごと | 1 時間〜1 日 | Builder のみ | `accumulated_fees` が増える。u64 オーバーフロー（極めて稀）まで機能的影響なし |
| オラクル publisher | オラクルごと | Slot ごと | 許可された publisher リスト | オラクルが staleness 窓口を超えて老いる。`PlaceOrderChecked` / `OpenPosition` / `Liquidate` がすべて拒否し始める |
| メンテナンス keeper | プログラムごと | 日次か週次 | プログラム管理者 or 無許可 | 休眠データが蓄積する。スキップしても安いが、最終的にゴミの rent を払う |
| Indexer | プログラムごと | リアルタイム | なし（読み取り専用） | フロントエンドが古い状態を表示する。分析が壊れる。ユーザが盲目になる |

表から見えるパターン 2 つ:

**許可制 vs 無許可。** 清算者とオラクル publisher（時に）は無許可 — 誰でも呼べ、任意の単一 keeper の停止に対してプロトコルが頑健。Vault NAV reporter と builder claim cron は定義上許可制 — 特定のエンティティだけが自分の状態に対して行動できる。ファンディング keeper は厳格さの度合いに応じて間に落ちる。

**Cadence vs レイテンシ許容度。** 清算者は sub-second でスキャンしなければならない、清算競争が速度に報いるから。25 slot（10 秒）古いオラクルは許容できる。Vault NAV は戦略のボラティリティに応じて分か時間の範囲。Builder claim は日待てる。誤った cadence を選ぶのが最も頻出の運用ミスの 1 つ — 攻撃的すぎれば手数料を浪費、怠慢すぎれば価値を出血させる。

---

## §14.2  ファンディング レート keeper

第 10 章 §10.5 はファンディング keeper を疑似 Python でスケッチした。本物のバージョンは 3 次元でより複雑だ: レートをどう計算するか、自身のダウンタイムをどう扱うか、複数 keeper をどう調整するか。

**レート計算。** 素朴な keeper は単に book mid とオラクル マークをサンプリングする:

```python
def compute_rate(market):
    mark = read_oracle_mark(market)         # ch.9
    bid, ask = read_top_of_book(market)     # ch.7
    mid = (bid + ask) / 2
    premium = (mid - mark) / mark
    return clamp(premium * K, -MAX_RATE, +MAX_RATE)
```

形は正しいが実務上 fragile。改善 2 つ:

1. **直近数分の book mid を TWAP する**、瞬時のスプレッドではなく。単一の transient な quote-stuffing 幅広スプレッドに基づいて funding を提出する keeper は、ユーザの equity を意味あるように動かすナンセンス レートを生みうる。本番 keeper は数秒ごとにサンプリングし 1〜5 分にわたって TWAP する。

2. **更新ごとの変化に上限。** 前のレートが 0.0001/sec で現行プレミアムが 0.001/sec を含意しても、1 更新で跳ねさせない — レート変化を更新あたり（例えば）50% にクランプする。これは制御系の意味でのレート制限。単一 keeper 呼び出しの暴走フィードバック ループ災害から守る。

本物の keeper 構造:

```python
class FundingKeeper:
    def __init__(self, market, program, signer):
        self.market = market
        self.program = program
        self.signer = signer
        self.history = collections.deque(maxlen=300)  # 1Hz で 5 min

    def tick(self):
        # 1. サンプル
        mark = read_oracle_mark(self.market)
        if mark is None:                # オラクル stale; この tick をスキップ
            return
        bid, ask = read_top_of_book(self.market)
        if bid is None or ask is None:  # 空板; スキップ
            return
        self.history.append((time.time(), mark, bid, ask))

        # 2. 計算 (history で TWAP)
        if len(self.history) < 60:      # 最低 1 min 必要
            return
        avg_mid = mean((b+a)/2 for _, _, b, a in self.history)
        avg_mark = mean(m for _, m, _, _ in self.history)
        premium = (avg_mid - avg_mark) / avg_mark
        target_rate = clamp(premium * K, -MAX, +MAX)

        # 3. 更新ごとの delta をクランプ
        last_rate = read_current_funding_rate(self.market)
        delta_cap = abs(last_rate) * 0.5 + MIN_DELTA
        new_rate = clamp(target_rate, last_rate - delta_cap, last_rate + delta_cap)

        # 4. 提出
        send_tx(UpdateFunding(new_rate), self.market, self.signer)

    def run(self):
        while True:
            self.tick()
            time.sleep(1)
```

**Keeper のダウンタイム。** Keeper がクラッシュしたら何が起きるか。オラクルの `MAX_ORACLE_STALENESS_SLOTS` が 25 slot（約 10 秒）でも、ファンディング keeper が 1 時間ダウンすれば、60 分前に設定されたファンディング レートが 60 分間適用され続ける。ロング/ショートはもう存在しない条件で設定されたレートでファンディングを支払う。

防御 2 つ:

- **Keeper にハートビート。** 運用監視（Grafana、PagerDuty）が N 分提出がなければ数分以内にアラートする。
- **オンチェーン ハンドラで最大経過時間を境界付ける。** `clock.unix_timestamp - last_update_ts > MAX_FUNDING_ELAPSED_SECONDS` なら `UpdateFunding` を拒否するチェックを加える。そういう場合、keeper か multisig が「reset」命令を先に呼ぶ。これは「静かに失敗するより大声で失敗する」パターン — keeper が回復した後に 1 年前のファンディングを適用するより、market を短時間壊すほうがましだ。

**複数 keeper。** ファンディング レート更新は**最後の**呼び出しのレートが適用されるという意味で冪等だが、同じ分内の複数呼び出しが大丈夫という意味では冪等ではない — それぞれが CU コストを加え、異なるレートを生む可能性がある。調整パターン:

- **単一 keeper、単一真理源。** 最も単純。1 プロセス、1 VPS、1 アラート。
- **Hot-standby。** Keeper 2 つ、1 つアクティブ。スタンバイは primary が N 分提出していないことを検出したら自分を昇格させる。Lock アカウント or オフチェーン リーダー選出で調整する。
- **クランプ付き無許可。** 誰でも `UpdateFunding` を呼べる。プログラムのクランプが griefing を防ぐ。複数 keeper が競争し、最初の 1 つが勝ち、2 つ目の tx は無害に失敗する（提出時にはレート読みがすでに古い）。一部の perp DEX が使う。

本章は permission を完全にオープンで出荷する（§10.3 — open-auth `process_update_funding` はテスタビリティのため意図的）。本番はオペレータの好みに基づいて上の 3 パターンから選ぶ。

---

## §14.3  清算 bot

第 11 章の `Liquidate` は無許可: 誰でも任意の水没ポジションに対して清算 tx を提出できる。経済が速度に報いる — 最初の清算者がペナルティを獲得するので、清算 bot はスキャン レイテンシと tx 提出速度で激しく競う。

清算者のループは構造的に単純だが運用上厳しい:

```python
def liquidator_loop():
    while True:
        positions = scan_all_positions()
        for pos in positions:
            mark = current_mark(pos.market)
            funding = current_funding_index(pos.market)
            equity = compute_equity(pos, mark, funding)
            notional = abs(pos.size) * mark
            maint = notional * MAINT_MARGIN_BPS / 10_000
            if equity < maint:
                send_tx(Liquidate(pos.pubkey), signer)
```

この innocent なループに隠れた運用問題 3 つ:

**スキャン レイテンシ。** 数千のポジション アカウントにわたる `scan_all_positions()` は安くない。RPC ベース `getProgramAccounts(programId, filter: discriminator)` は遅く（数百 ms から秒）、リアルタイムではない。本番清算者は Geyser プラグインや RPC pubsub を使ってリアルタイムでポジション アカウント更新を受け取り、各オラクル/ファンディング tick で health を再計算する全ポジションのインメモリ ミラーを保つ。

**競合条件。** 複数の bot が同じ清算可能ポジションを見る。最初に tx をランディングした 1 つが勝つ。残りは失敗 tx の手数料を払う（オンチェーン `Liquidate` はすでにクローズされたポジションを拒否する）。Bot が競う方法:

- **事前構築 tx。** ポジションが閾値を割った瞬間に `Liquidate` tx を構築する。提出時に新鮮な blockhash と署名だけを取る。ミリ秒節約。
- **Jito 経由か直接リーダー RPC への提出。** Public RPC には計測可能な遅延がある。専門インフラがそれを削る。
- **Priority fee。** 競合 slot で最初にランドするため追加で支払う。利益を残したまま最高 priority fee を払う清算者が勝つ。

**収益性。** 清算は `notional × LIQUIDATION_PENALTY_BPS / 10000 = notional × 0.01`（第 11 章の値）を支払う。清算者は収益 > (tx コスト + RPC コスト + インフラ コスト + bot 運営に縛られる資本の機会コスト) を必要とする。$0.001 / Solana tx で、$1000 notional ポジションの清算成功は $10 を支払う — 楽に収益性。$10 notional ポジションは $0.10 を支払い、ほとんどの運用閾値以下 — 小さな水没ポジションは競争が少なくなり、水没のままより長く座り、（担保がすでにゼロなら）清算するのは正味マイナスだ。本番設計は時にこれを避けるためポジションあたり最小サイズを加える。

現代の Solana 清算者アーキテクチャ:

```
   バリデータ Geyser プラグイン / RPC pubsub
              │
              ▼
   ローカル ポジション ミラー (インメモリ)
              │
              ▼
   Health コンピューター (オラクル/ファンディング更新でトリガ)
              │
              ▼
   清算候補キュー (期待収益でソート)
              │
              ▼
   Tx ビルダ + 提出者 (Jito MEV searcher か直接リーダー RPC)
```

複数の競合清算者が本質的に同一のスタックを走らせる。差別化はレイテンシ、priority fee チューニング、小さなアルゴリズム的優位性（例: 次のオラクル tick でどのポジションが清算可能になるかを予測し、それらの tx を事前構築する）にある。

---

## §14.4  Vault NAV reporter

第 12 章の `UpdateNAV` はマネージャのみで信用される。Keeper はだからマネージャが走らせるプロセス、正しくすべき点が 2 つ: cadence と精度。

**Cadence。** 頻繁すぎると預金者が noise で跳ねる NAV を見る（マネージャは実際にはポジションを変えていないが、根底のオラクルが動いたから keeper が更新する）。怠慢すぎると deposit/withdraw が数分古い NAV で値付けされ、遅いムーバーに価格変動の無料オプションを与える。

典型的パターン:

- **高頻度 vault (HFT、market making):** 毎分。Cadence が deposit/withdraw タイミング arbitrage を無視できる程度にリアルタイム近似する。
- **中頻度 vault (トレンド フォロー、モメンタム):** 5〜15 分ごと。マネージャのポジションが実際に変わるのに十分。
- **低速 vault (yield アグリゲータ、basis 取引):** 1 時間ごと or epoch 境界ごと。

微妙な設計選択: keeper は**意味ある変化があるときだけ** NAV を更新すべきか、常に更新すべきか。「常に」は預金者に予測可能な cadence と「はい、マネージャはまだ報告している」の可視性を与える。「意味あるときだけ」は tx 手数料を節約する。ほとんどの本番 vault はハイブリッドを選ぶ: 少なくとも N 時間に 1 回は更新する（heartbeat）、変化が閾値を超えたらより早く更新する。

**精度。** マネージャはオフチェーンで NAV を計算する — vault のオープン ポジション equity の合計（第 11 章の `compute_equity` を使う）、加に現金担保、減に保留中の手数料。この計算は、同じ入力を与えられたときオンチェーン ハンドラが計算するものと一致しなければならない。さもなければ預金者は報告された NAV と実際に受け取るものの間にドリフトを見る。

リスク領域 3 つ:

- **ファンディング蓄積ドリフト。** 最後の NAV レポート以降 `UpdateFunding` が呼ばれていないなら、ポジションのファンディング PnL は古い指数に対して計算される。本番 keeper は `UpdateNAV` を呼ぶ**前に**該当 market の `UpdateFunding` を呼び、NAV が蓄積ファンディングを正確に反映するようにする。
- **オラクル staleness ドリフト。** オラクルが更新されていないなら、`compute_equity` で使うマークが古い。同じ修正: NAV 更新前にオラクルを refresh する。
- **未実現 vs 実現。** オープン ポジションを持つ vault には未実現 PnL があり、マーク価格に依存する。大部分をクローズした vault には現金に座る実現 PnL がある。Keeper は両方を正しく計算し、部分クローズを二重カウントしてはならない。

ここが本番でしばしばマネージャが keeper を**外注**する部分（Squads マルチシグ + 自動 NAV スクリプト、あるいは Lulo や Kamino の vault SDK のような vault 管理プラットフォーム）。自分でやるには運用信頼性問題を所有することが必要だ — keeper 停止 = 古い NAV = 不幸な預金者。

---

## §14.5  Builder claim cron

最も単純な keeper。Builder の `accumulated_fees` は `ClaimBuilderFees` を呼ぶまで単調増加する。Keeper は cron ジョブだ:

```python
def claim_loop():
    while True:
        profile = read_builder_profile(my_pubkey)
        if profile.accumulated_fees >= CLAIM_THRESHOLD:
            send_tx(ClaimBuilderFees(), my_pubkey)
        time.sleep(CLAIM_CHECK_INTERVAL)
```

パラメータ 2 つとボイラープレート 1 つ。

**`CLAIM_THRESHOLD`。** 各手数料が小さくても、すべての手数料を claim するな。Claim tx は Solana 手数料で約 $0.001 かかる。蓄積手数料が $0.005 なら、20% を claim に吹き飛ばしたことになる。閾値を claim コストが claim 額の数 % 未満になるよう十分高く設定 — 通常、数ドル相当の蓄積手数料。

**`CLAIM_CHECK_INTERVAL`。** 毎時で寛大。Claim に緊急性はない — 手数料は盗まれず、インフレで蝕まれず（インフレなし。すべて u64 quote 単位）、ただ座る。一部の builder は毎日 claim、他は毎週、他は毎月。

**ボイラープレート。** 詰まった claim ジョブ（例: ウォレットが tx 手数料用 SOL を切らした）が黙って手数料を永遠に蓄積させないよう監視を設定する。運用上は些細だが忘れやすい。

これがシステムで最も低リスクな keeper。完全性のため言及するのが第一だが、操作上の運用が新しいなら最初に書く良い keeper でもある、失敗モード（claim されない数ドル）が穏やかだから。

---

## §14.6  オフチェーン indexer

厳密には keeper ではないが必須。Indexer はチェーン状態を購読し、処理し、結果をフロントエンド、分析ツール、アラートに公開する。

アーキテクチャ選択 3 つ。

**(1) Geyser プラグイン。** Geyser は Solana のバリデータ側ストリーミング インターフェース。Geyser プラグインはバリデータ内部で走り、すべてのアカウント変更、トランザクション、slot イベントをリアルタイム、チェーン commit から sub-millisecond レイテンシで受け取る。利点: 最低レイテンシ、完全なデータ。欠点: 自前バリデータを走らせる必要（あるいはプラグインを走らせるノード オペレータと提携する）、運用の複雑さ、ハードウェア コスト。

大型 DEX の本番 indexer はほぼ常に Geyser を使う。Helius、Triton、その他 Solana-RPC プロバイダがバリデータ運用負担を避けるため Geyser-as-a-service を提供している。

**(2) RPC pubsub。** WebSocket ベース RPC pubsub インターフェース（`accountSubscribe`、`programSubscribe`、`logsSubscribe`）でアカウント変更を購読する。利点: セットアップ簡単、WebSocket クライアント以外のインフラ不要。欠点: レイテンシが高い（数百 ms）、接続が落ちる、再接続中に一部のイベントが取り逃される可能性がある。

中リスク ユース ケースには良い: 数秒ごとにユーザ向け状態を更新するフロントエンド、日次ボリュームを計算する分析サービス。高頻度用途（清算 bot、市場メイク bot）には不十分。

**(3) RPC polling。** ループ内の `getProgramAccounts` + `getTransaction`。Pubsub が選択肢でないときのフォールバック（開発、デバッグ、単純な bot）。利点: 最大限単純。欠点: 高レイテンシ、RPC 呼び出しコスト高、多くのアカウントで悪くスケールする。

このプログラム向けの典型的本番アーキテクチャ:

```
   Geyser ストリーム (アカウント変更 + tx ログ)
        │
        ▼
   イベント ディスパッチャ (プログラム ID、アカウント discriminator で ルート)
        │
        ├─► ポジション ミラー (清算 bot 用)
        │
        ├─► Vault NAV キャッシュ (フロントエンド用)
        │
        ├─► 取引履歴 DB (分析用)
        │
        ├─► アラート エンジン (オラクル staleness、vault NAV staleness、
        │                       builder 手数料蓄積、...)
        │
        └─► WebSocket fan-out (ユーザ イベントを購読するフロントエンド用)
```

Indexer はオフチェーン スタックの肝心なピースだ — 上記のすべての keeper が暗黙にチェーンに対する高速で正しい状態クエリを持つことに依存し、indexer がそれを提供する。

オンチェーンではなくオフチェーンで計算すべきもの（第 5 章のレッスン、indexer 特化で言い直し）:

- **market ごとの取引総ボリューム。** オフチェーン。tx ログから計算が安い。
- **総建玉 (open interest)。** オフチェーン。すべての Position アカウントの notional の合計。
- **market ごとの活動、top-of-book 履歴、約定価格テープ。** オフチェーン。
- **share あたり NAV、vault の歴史的 PnL。** オフチェーン。`UpdateNAV` ログ イベントから計算。
- **Builder ボリューム ランキング。** オフチェーン。BuilderProfile 読みから。

オンチェーン プログラムは自分の不変条件を強制するために必要なものだけを保持する。あるとよいが信用境界の一部でないものはすべて indexer に住む。

---

## §14.7  まとめ — 組み立てたもの、それで何をするか

14 章をかけて組み立てたもの:

**Phase A (基礎、ch.1–5):** アカウント モデル、ネイティブ プログラム、PDA、コンピュート バジェット、Sealevel 並列性。それ自体で立つ完全な「Solana from scratch、Anchor なし」カリキュラム — Phase A を終えた学習者は、フレームワークに一度も触れずに本物の Solana プログラムを書き、本物の Solana プログラムをデバッグできる。

**Phase B (HL プリミティブ、ch.6–14):** CPI 経由の SPL Token vault、オンチェーン板、マッチング エンジン、オラクル統合、ファンディング レート蓄積子、清算付きポジション ライフサイクル、プール取引 vault、builder code、結果を走らせるオフチェーン インフラ。動く — スコープが繰り延べられているが — perp DEX、すべての設計選択が注釈付きで、何が繰り延べされたかについての正直なメモが章に書き込まれている。

明示的に繰り延べたもの（だから後で見つけやすい）:

- **builder 手数料の SPL Token エスクロー**（ch.13）。Ch.11 と ch.12 は本物のエスクローを出荷する。3 者間の手数料分割はそれらの 2 者間移動とは構造的に異なり、§13.5 は実装ではなく設計問題として fee-vault アーキテクチャを示す。
- **Slab ベース板。** Ch.8 はページング付きフラット配列マッチャを出荷し §8.4 で slab を擬似コード化する。実装はよく scoped された宿題。
- **保険基金。** Ch.11 が議論する（§11.6）。コードにはない。水没クローズを正しく社会化する前に必要。
- **本物の Pyth 統合。** Ch.9 はモックを使う。§9.5 の移行表が本物の Pyth に切り替える 1 ページ diff。

本物の本番デプロイのために追加するもの:

- 監査。最低 3 ヶ月のセキュリティ レビュー。
- 本章が参照したすべての定数（margin BPS、fee BPS、cap）に multisig 管理者権限。
- 現在プログラム全体でグローバルな定数に market ごとの設定。
- プロジェクトが管理者制御ではなくコミュニティ制御を意図しているならガバナンス プログラム。
- 監視、アラート、ランブック付きの本章の完全 keeper スタック。
- フロントエンドと indexer。
- 法務レビュー（どの管轄が運営、上場、ユーザになれるか）。

カリキュラムで何をするか:

- **使う。** 教材コードは MIT ライセンス。Fork し、デプロイし、拡張する。教育的バリアント（Bench、Stats）を落として実際のプリミティブを出荷する。
- **教える。** 章は SolDojo internals トラックのために設計された。ワークショップを走らせ、コースを組み立て、後続を書く。教育的フレームワーク（検証済み章フォーマット、明示的な正直なメモ付き意図的スコープ繰り延べ、2 言語カバレッジ）は他の Solana 主題にも一般化する。
- **批判する。** どの章も別の設計者なら違うように作る設計選択を持つ。独自バージョンを組み立て、自分の divergence を反カリキュラムとして書き上げる。

ピースはあなたの手にある。

---

## ここにないもの（意図的に）

これは閉じる章であって、ロードマップではない。書かなかったもの:

- 「次に読むもの」リーディング リスト。Phase A と Phase B の章は関連する場所で Solana ドキュメント、Pyth ドキュメント、ソース コードを引用する。それが正典的な追加リーディングだ。
- 特定の Solana エコシステム方向への売り込み。チェーンがここからどこに行くかは、そこに組み立てている人々が決めること。本トラックは strategy ではなく substrate を教える。
- 「ありがとう」や「また次回」。カリキュラムの終わりは読み手のもので、書き手のものではない。この 14 章から何かを得たなら、次にあなたがやる価値あることは何かを出荷することだ。行け。
