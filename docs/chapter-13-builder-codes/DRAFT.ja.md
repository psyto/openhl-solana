# 第13章 — プロトコル プリミティブとしての Builder Codes

> 状態: ドラフト (v0.1)。
> 教材コード: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs)（`BuilderProfile`）、[`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（`process_register_builder` 2604–2670 行、`process_place_order_with_builder` 2680–2817 行、`process_claim_builder_fees` 2819–2860 行）、[`scripts/builder/src/main.rs`](../../scripts/builder/src/main.rs)。

---

## §13.0  はじめに — builder code とは何で、何でないか

**Builder code** は取引に付与されるフロントエンド単位の識別子だ。ユーザがフロントエンド経由で注文を開くと、フロントエンドは builder code をトランザクションに含める。プログラムは取引のプロトコル手数料の設定可能な一部を、その builder のオンチェーン アカウントに credit する。

Hyperliquid が現行の意味でこの語を造った。フロントエンドが自分で板を運営せずに注文フローをマネタイズできるようにする、プロトコル ネイティブの仕組みだ。「ルータ手数料」（Uniswap）、「ホワイトラベル ルーティング」（CEX）、「紹介ブローカー」（TradFi）と同じ系譜 — 長く存在する分配インセンティブの Solana 版だ。

builder code は次のものでは**ない**:

- **紹介コード (Referral codes)。** 紹介コードは新規ユーザを紹介した人に報酬を与える。通常、サインアップごとに 1 回、あるいは紹介相手の手数料の長い尾を比率で永遠に支払う。Builder codes は**取引**ごとに支払い、誰が誰を紹介したかは追跡しない — ルーティングに報い、紹介に報いるのではない。
- **メーカー/テイカー リベート。** メーカー リベートはユーザ（指値注文を出した人）に自分の手数料の一部を戻す。Builder codes はユーザの手数料の一部を**第三者**（フロントエンド）に支払う。ユーザはどちらにせよ同じ総手数料を払う。違うのは分配を誰が受け取るかだ。

本章は 3 つの命令を出荷する。

1. **`RegisterBuilder`** — 各 builder が、累積手数料と自己宣言した最大シェアを保持する builder ごとの `BuilderProfile` PDA を作る。プログラムが credit するアカウントが必要なので登録が必要。アカウントなし、手数料なし。
2. **`PlaceOrderWithBuilder`** — 4 つ目のアカウントとして builder profile を取る取引命令バリアント。プロトコル手数料を計算し、builder のシェアを profile の `accumulated_fees` に分割し、`PlaceOrderChecked` と同じ place-order パスを走らせる。
3. **`ClaimBuilderFees`** — builder の引き出し呼び出し。アキュムレータをゼロ化して額をログに残す。第 11 章・第 12 章 — 実際の SPL Token エスクローを追加した — とは異なり、本章は手数料分割についてはトークンを動かす手前で意図的に止める。§13.5 が、なぜ builder 手数料エスクローはポジションや vault のエスクローとは別種の設計問題なのか、本番実装がどのような形になるかを説明する。

本章の知的内容は 3 部構成だ。§13.4 のアトミシティ論（なぜ手数料分割は取引命令の中で起き、別の「取引ごとに claim」呼び出しではないか）、§13.2 のキャップ積み重ねパターン（自己宣言キャップとプロトコル レベル キャップがどう相互作用し、builder が侵害されても手数料漏れを境界付けるか）、そして §13.5 の本番エスクロー設計議論（なぜ builder 手数料は第 11 章・第 12 章の二者間移動より構造的に難しいエスクロー問題なのか）。

---

## §13.1  `BuilderProfile` アカウント

`crates/state/src/lib.rs` から:

```rust
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct BuilderProfile {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub builder: [u8; 32],          // builder の pubkey
    pub max_fee_share_bps: u64,     // 自己キャップ、登録時にクランプ
    pub accumulated_fees: u64,      // claim 対象
    pub total_volume: u64,          // 統計: この builder 経由でルートされた base size
    pub _reserved: [u8; 32],
}
```

104 バイト。意味のあるフィールド 4 つ:

**`builder`** はフロントエンド / アグリゲータの pubkey。PDA 派生での唯一のシードに使う: `[BUILDER_PROFILE_SEED, builder.key]`。これですべての Solana pubkey は高々 1 つの builder profile を持ち、pubkey を知っている誰もが派生できる。Profile は openhl-core 所有（本プログラムだけが `accumulated_fees` を変更できる）だが、誰でも公開フィールド（`builder`、`total_volume`）を**読み**、builder の主張するルーティング量を検証できる。

**`max_fee_share_bps`** は builder の自己宣言した、プロトコル手数料のどの fraction を取るかの上限。`max_fee_share_bps = 2000` で登録した builder は、ルートされる任意の注文でプロトコル手数料の最大 20% を取ると公にコミットしている。これは信用シグナル — 「20% は我々、80% はプロトコル」と「50/50」を宣伝する builder は、ユーザにフロントエンドがどうマネタイズするかを伝える。自己キャップが低いほど → ユーザに優しい手数料分割 → 潜在的に多いフロー。

**`accumulated_fees`** は builder に credit され claim を待つ手数料の走行合計。このプロファイル経由でルートする `PlaceOrderWithBuilder` ごとにインクリメント。`ClaimBuilderFees` でゼロにリセット。Builder はプログラム アカウント内に蓄積しバッチで引き出す — 取引ごとに claim するよりはるかに安価だ。

**`total_volume`** は観測性 — builder 経由でルートされた base size。Builder が潜在パートナーに自分のフローを証明するため（あるいはユーザが builder の実績を吟味するため）使う。プログラム ロジックには使わない。

注目すべき設計制約 2 つ:

- **Builder あたり 1 profile。** PDA 派生が (builder pubkey) → (profile pubkey) マッピングを全単射にする。Builder は market ごとに異なる手数料分割の 2 profile を持てない。A/B テストしたいなら 2 つの異なる builder ウォレットを使う。
- **Quote 単位での手数料蓄積。** `accumulated_fees` はプロトコル手数料が取られる quote 通貨で計上される。複数 quote 通貨（USDC と USDT）なら、設計は (builder, mint) ごとの profile が必要、builder ごとだけではなく。ここは意図的に単一 quote に保つ、プログラムの残りも単一 quote なので。

> **演習 §13.1.** あるフロントエンドが `max_fee_share_bps = 3000` で登録する。3 つの取引が経由する: notional 1000、2500、700。`PROTOCOL_FEE_BPS = 10`（0.1%）で、3 取引すべての後の `accumulated_fees` 合計は?

---

## §13.2  キャップ 2 つ、積み重ね

Builder code は 2 つの手数料シェア キャップが要る、1 つではなく、2 つの当事者が異なるインセンティブを持つから:

**プロトコル キャップ (`PROTOCOL_BUILDER_SHARE_CAP_BPS = 5000`)** — **任意の** builder が任意のプロトコル手数料の留保できる最大 fraction。プログラムにハードコード。本書の値 5000 bps だと、プロトコルはすべてのプロトコル手数料の少なくとも 50% が builder 設定に関わらずプロトコルに残ることを保証する。

**Builder 自己キャップ (`BuilderProfile.max_fee_share_bps`)** — この**特定の** builder が留保する最大 fraction。登録時に自己宣言、ユーザに見える。

任意の取引での実効シェアは `min(builder.max_fee_share_bps, PROTOCOL_BUILDER_SHARE_CAP_BPS)`。`programs/openhl-core/src/lib.rs:2742–2748` から:

```rust
share_bps = profile.max_fee_share_bps;
if share_bps > PROTOCOL_BUILDER_SHARE_CAP_BPS {
    // 防御的: 登録済み profile はキャップを超えるべきではないが、
    // builder 登録以降にキャップが下げられた可能性がある。
    share_bps = PROTOCOL_BUILDER_SHARE_CAP_BPS;
}
```

プロトコル キャップは登録時にクランプする（ユーザはキャップ以上を要求できない）が、取引時にも再クランプする — 登録と取引の間にキャップが下げられうるからだ。（本章は定数を出荷する。本物のプログラムは `PROTOCOL_BUILDER_SHARE_CAP_BPS` を config アカウント上に住むガバナンス調整可能値にするかもしれない。まさにそこで防御的再クランプが重要になる。）

2 キャップ構造はコアな安全性プロパティだ。扱う 3 つの失敗モード:

1. **悪意ある builder。** 何らかの形で登録時に `max_fee_share_bps = 10000`（手数料の 100%）を宣言する builder は即座に `PROTOCOL_BUILDER_SHARE_CAP_BPS` にクランプされる。プロトコルは常にフロアを保つ。
2. **ユーザのミス。** 馴染みのない builder にルートするユーザでも、取引前に**最大**の手数料漏れを知っている — 両キャップをオンチェーン状態から読める。手数料の驚きはない。
3. **侵害された builder。** Builder のウォレットが侵害され、攻撃者がシェアを膨らませようとしても、登録キャップ（登録後は不変）を超えられない — 再登録すれば新しい PDA が異なるアドレスに生まれ、既存のフローが壊れる。

本番 builder プログラムはしばしば**第 3 の**キャップを追加する — プロトコルが異なる market で異なる手数料分割を charge できる、market ごとや資産ごとのキャップ。本章は明快さのために省略するが、パターンは自然に拡張する。

> **演習 §13.2.** Builder が `max_fee_share_bps = 8000` で登録される。キャップ積み重ねを通れ: 本書の `PROTOCOL_BUILDER_SHARE_CAP_BPS = 5000` で、実効シェアは? Builder 登録後にガバナンス投票がプロトコル キャップを 3000 に下げた**ら** — 次の取引でその profile を通したら何が起きるか?

---

## §13.3  `PlaceOrderWithBuilder` を歩く

`programs/openhl-core/src/lib.rs:2680–2817` の `process_place_order_with_builder`。ハンドラは `PlaceOrderChecked` の厳格な上位集合 — 同じチェック、同じ place ロジック — に手数料分割ブロックを検証と注文書き込みの間に挿入。

**検証 + サニティ バンド**（2693–2724 行）: `PlaceOrderChecked` と同一、`accounts` に追加の `builder_profile_ai` スロットを除いて。オラクル staleness チェック、オラクル マークに対する価格サニティ バンド — すべてそのまま持ち越し。

**手数料計算 + builder credit**（2726–2775 行）:

```rust
let notional_val = (price as u128) * (size as u128);
let protocol_fee = (notional_val * (PROTOCOL_FEE_BPS as u128) / 10_000) as u64;

// ... builder profile を読み、share_bps をキャップでクランプ ...

let builder_share = ((protocol_fee as u128) * (share_bps as u128) / 10_000) as u64;
profile.accumulated_fees = profile.accumulated_fees.checked_add(builder_share)?;
new_volume = profile.total_volume.checked_add(size)?;
profile.total_volume = new_volume;
```

3 つの数値が落ちる:

- `notional_val = price × size` — 取引の総価値、quote-unit-scaled 形式で。
- `protocol_fee = notional × 10 / 10_000` — notional の 0.1%、quote 単位。
- `builder_share = protocol_fee × share_bps / 10_000` — プロトコル手数料の fraction、quote 単位。

`builder_share` は `checked_add` で `profile.accumulated_fees` に追加（オーバーフローは静かにキャップする代わりに取引を拒否 — builder は走行合計を u64::MAX 以下に保つため定期的に claim できる）。`total_volume` は観測性のため取引サイズ分進む。

残りの `protocol_fee - builder_share` はプロトコルが留保する。スコープ繰り延べ版ではこれは暗黙のまま（どこにも追跡しない）。本番では SPL Token CPI でプロトコル手数料 vault アカウントに転送される。本章の教育的論点はどちらにせよ着地する: 分割は取引とアトミックに起きる。

**注文配置**（2780–2811 行）: §9.4 の `PlaceOrderChecked` と同一。空スロットを線形走査、注文を書く、カウンタをインクリメント。`PlaceOrderWithBuilder` の CU コストは `PlaceOrderChecked + 約 600 CU`、手数料計算と builder profile 借用のため。

> **演習 §13.3.** `price × size` が大きすぎて `notional × PROTOCOL_FEE_BPS` が `u128` をオーバーフローする `PlaceOrderWithBuilder` が呼ばれたら何が起きるか。失敗パスを辿れ。なぜこの計算に `u64` ではなく `u128` 精度が正しいか?

---

## §13.4  アトミシティ論

`PlaceOrderWithBuilder` は手数料分割を**注文配置と同じ命令内**で行う。「各取引の後、builder が別の `RecordFee(trade_id, amount)` 命令を呼ぶ」パターンはない。アトミシティが肝心な理由は 3 つ:

**1. 決済の誠実性。** 手数料分割が別トランザクションで起きるなら、ユーザは slot N で取引手数料を支払い、builder は slot N+1 でシェアを受け取れない可能性がある（アカウントがクローズされた、キャップが変わった、等）。アトミシティは: 取引が分割適用で commit するか、どちらも起きないかのいずれか、を意味する。「手数料は払ったが builder は credit を受け取らなかった」失敗モードはない。

**2. CU 効率。** 別の「record fee」命令はもう 1 トランザクションぶんの手数料、ネットワーク往復、CU オーバーヘッドを毎単一取引でかかる。1 日あたり数千取引 × builder ごとで、相当のコストになる。インライン蓄積は取引ごとに 1 回、claim は N 取引ごとに 1 回、総コストは償却される。

**3. スケジューリング。** 別の手数料記録命令は builder profile をすべての取引トランザクションで書き込み可能アカウントとして要求する、分割 == 0 でも。Sealevel はその後、同じ builder の profile 上のすべての取引を直列化する（第 5 章のアンチパターン）。分割が取引内なら、**その builder にルートされた取引のみ**が profile に触れる — だから 2 builder のフローは同じ market を含んでも並列に処理できる。

**蓄積**（取引とアトミック、`PlaceOrderWithBuilder` 内）と **claim**（バッチ、別の `ClaimBuilderFees` 呼び出し）の分割も肝心だ。Claim は高価な操作: 実際のトークンを動かす必要があり（本番）、それは SPL Token CPI、それは signer セットアップとアカウント検証を意味する。それを取引ごとに行うのは無駄だ。Profile に蓄積させ builder に定期的に（時間ごと、日ごと、何でも）claim させることで、取引ごとのコストは最小限に保たれる。

この蓄積 - バッチ - claim パターンは Ethereum の ERC-20 配当分配と同一 — 同じ問題、同じ解、異なるランタイム。

> **演習 §13.4.** 各取引が別の `RecordFee` 命令を発行し builder が処理しなければならない代替設計を草案せよ。負荷下（1 builder 経由で 10 取引/秒など）での BuilderProfile アカウントへの秒あたり書き込み数を数えよ。本書の設計と比較せよ。どちらが Sealevel をより積極的に直列化するか?

---

## §13.5  `RegisterBuilder` と `ClaimBuilderFees`

両方とも短い。`RegisterBuilder`（2604–2670 行）は第 3 章の標準 PDA 作成パターン、1 つのひねり付き: 要求された `max_fee_share_bps` は登録時にクランプされる:

```rust
if max_share > PROTOCOL_BUILDER_SHARE_CAP_BPS {
    msg!(
        "register_builder: requested {} bps clamped to protocol cap {} bps",
        max_share,
        PROTOCOL_BUILDER_SHARE_CAP_BPS
    );
    max_share = PROTOCOL_BUILDER_SHARE_CAP_BPS;
}
```

クランプは静か — 登録は成功する、シェアが下げられただけだ。クランプをログすると、builder はプログラム ログから実効キャップを検証できる。

`ClaimBuilderFees`（2819–2860 行）はさらに単純:

```rust
if profile.builder != *builder_ai.key.as_ref() {
    return Err(ProgramError::IllegalOwner);
}

let claimed = profile.accumulated_fees;
profile.accumulated_fees = 0;

msg!(
    "claim_builder_fees: builder {} claimed {} units ...",
    builder_ai.key,
    claimed
);
```

認可（builder だけが自分の手数料を claim できる）、アキュムレータをゼロ化、ログ。本番ではここで SPL Token Transfer CPI が、プロトコル手数料 vault トークン アカウントから builder のトークン アカウントへ、`claimed` quote トークンを PDA 署名で動かす。

本物の本番 claim は通常、部分引き出し（`claim --amount N`、常に全部ではなく）、蓄積子タイムアウト（>N 日アイドルな手数料はプロトコルに没収）、プロトコルが複数 quote 通貨をサポートするときの per-token claim もサポートする。どれも根本的な形を変えない。上に乗るポリシー判断だ。

> **演習 §13.5.** `process_claim_builder_fees` を編集して、ペイロードに `partial_amount: Option<u64>` を受け入れるようにせよ。`Some(n)` なら `min(n, accumulated_fees)` を claim。`None` ならすべて claim。引き出せる総額が同じでも、部分引き出しパターンが builder に有用な理由は?

### 本番エスクロー設計ノート — なぜこれは宿題として残すか

第 11 章と第 12 章はポジション担保と vault deposit に実際の SPL Token エスクローを追加した。本章は追加しない。理由は、builder 手数料エスクローは、それらの章が必要とした二者間トークン移動とは**構造的に違う**エスクロー問題だからだ。具体的な単一実装を示すことは教えるより誤解させる方が大きい。配線を書き始める前に 4 つの設計判断が必要になる。

**1. 二者 vs 三者。** ポジション担保と vault deposit は二者間移動だ: ユーザ ↔ vault、片方が署名し、経済判断（いくら、誰の署名で）はすべて命令にエンコードされている。Builder 手数料は**三者分割**だ: ユーザは単一のプロトコル手数料を払い、そのうち一部がプロトコルへ、別の一部が builder へ行く。1 つのユーザ支払いを 2 つの受取人にアトミックに分割し、各々に正しい比率を credit するには、`spl_token_transfer_user_signed` や `spl_token_transfer_vault_signed` が提供する形とは別の形が必要だ。自然な実装は、quote mint ごとに 1 つの**手数料 vault** トークン アカウント、`[b"fee_vault", quote_mint]` のような PDA 所有。各 `PlaceOrderWithBuilder` は (a) ユーザのトークン アカウントから手数料 vault へ**全額**の `protocol_fee` を Transfer し、(b) builder のアキュムレータに `builder_share` を credit する。`(protocol_fee - builder_share)` の残余は手数料 vault に残り、プロトコルの取り分になる。

**2. `PlaceOrderChecked` 非対称性。** 手数料エスクローを `PlaceOrderWithBuilder` だけに追加すると、ねじれたインセンティブが生まれる: builder なしの経路（`PlaceOrderChecked`）はトークン建ての手数料をまったく取らないので、builder 経由の取引は同じ取引より実質コストが高くなる。自然な解は、*すべての* place-order バリアントにプロトコル手数料エスクローを入れること — だがそれは §13.3 を破綻させずに本章で着地できるより大きな取引パスへの外科手術だ。本番デプロイは初日からプロトコル手数料を両バリアント共通として扱うことでこれを解く。

**3. 複数 quote 対応。** 単一 quote 通貨（本章のケース）なら手数料 vault は単一アカウント。複数 quote 通貨では、(quote_mint) ごとの手数料 vault *かつ* (builder, quote_mint) ごとのアキュムレータが必要 — `BuilderProfile` はマップ フィールドを持つ（Pod 適合しない）か、quote ごとに別 profile PDA に分かれる。§13.1 で単一 quote 制約を意図的だと述べた。本番エスクロー設計こそが、それが実際にアカウントを要求し始める場所だ。

**4. Claim 側の authority。** 手数料 vault が存在すれば、`ClaimBuilderFees` は手数料 vault authority PDA で署名された `fee_vault` → `builder_token` の PDA 署名 Transfer になる。§12.4 の `VaultWithdraw` と構造的に同一 — 同じ `invoke_signed` パターン、同じ `InvalidSeeds` 保護、違う seeds。これだけが、本章がすでにやった作業の「機械的拡張」と呼べる部分だ。

本章が実際に教える仕組み — 取引とアトミックな accrual、別バッチ操作としての claim、二段キャップ安全性 — は 4 つの設計判断すべてを通じて変わらない。差分は SPL Token 配線だけだ。

> **演習 §13.5b（設計）.** 本番 `ClaimBuilderFees` のアカウント レイアウトをスケッチせよ: どのアカウントが（順に）渡され、どれが signer で、どれが PDA で、どの seeds で派生するか。コードを書く必要はない — `scripts/builder/src/main.rs` に現れる `accounts: vec![...]` 宣言だけでよい。§12.4 の `VaultWithdraw` 宣言と比較せよ: 構造的に同一なのは何か、構造的に違うのは何か、各々の違いは上の 4 つの設計判断のどれに起因するか?

---

## §13.6  まとめと自己検証

### まとめ図

```
Builder ライフサイクル:

  1) Builder 登録
     RegisterBuilder(max_fee_share_bps=2000)
     ──► BuilderProfile{ builder, max_share=2000, fees=0, vol=0 }


  2) ユーザが builder 経由で取引
     PlaceOrderWithBuilder(side, price, size)
     accounts: [user(S), book(W), oracle(R), builder_profile(W)]

       notional       = price × size
       protocol_fee   = notional × 10 / 10000    (PROTOCOL_FEE_BPS)
       share_bps      = min(profile.max, 5000)    (PROTOCOL キャップ)
       builder_share  = protocol_fee × share_bps / 10000

     アトミック:
       book に注文配置                            ─┐
       profile.accumulated_fees += builder_share   ├─ 同じ tx、同じ slot
       profile.total_volume     += size            ─┘


  3) Builder が定期的に claim
     ClaimBuilderFees(empty)
     ──► profile.accumulated_fees = 0
     ──► (本番: SPL Token CPI が claim された額を動かす)


2 キャップ安全性:

  effective_share = min( builder.max_fee_share_bps
                       , PROTOCOL_BUILDER_SHARE_CAP_BPS
                       )

  ↑ builder が最大自己キャップにコミット（ユーザに見える）
  ↑ プロトコルがキャップのキャップにコミット（ハードコード / ガバナンス）

  プロトコル取り分のフロア = (10000 - 5000) × PROTOCOL_FEE_BPS / 10000
                          = notional の 5 bps、常に留保
```

### 自分で検証する 3 項目

1. **キャップが正しく積み重なる。** `--max-share-bps 9999` で builder を登録する。ハンドラはクランプをログし、ダンプは `max_fee_share_bps = 5000`（本書の `PROTOCOL_BUILDER_SHARE_CAP_BPS`）を示すはずだ。次に取引をその builder 経由でルートする — builder のシェアはちょうどプロトコル手数料の 50% のはずだ。
2. **失敗下でアトミシティが保たれる。** シミュレートされた失敗を使う: stale なオラクルで `PlaceOrderWithBuilder` を試す（注文配置が失敗する）。シミュレーションは `oracle stale` で失敗し、**かつ**失敗 sim 後 builder profile の `accumulated_fees` は不変のはずだ（トランザクション全体が revert するので）。取引が起きない限り分割は起きない。
3. **取引ごとに volume が蓄積する。** 同じ builder 経由で 5 つの注文をサイズ 10、20、30、40、50 で配置する。`total_volume` はちょうど 150 （10+20+30+40+50）のはずだ。`accumulated_fees` が `(notional_total × PROTOCOL_FEE_BPS × share_bps / 10000 / 10000)` に加算されないなら、数学に off-by-one がある — それを追いかけよ。

---

## 第 14 章への導線

本番デプロイが必要とするすべてのプリミティブを扱う perp DEX プログラムを持つようになった: アカウント、プログラム、PDA、CPI、コンピュート、並列性、vault、板、マッチャ、オラクル リーダ、ファンディング、ポジション、清算、プール取引 vault、builder code。**まだ持っていない**のは、それを走らせ続けるオフチェーン配管だ。

第 14 章 — Cranks、Keepers、オフチェーン グルー — が Phase B とトラックを閉じる。本プログラムが暗黙に要求するすべての keeper を歩く: ファンディング レート keeper（第 10 章の hook）、清算 bot（第 11 章の無許可 `Liquidate`）、vault NAV reporter（第 12 章の `UpdateNAV` cadence）、builder claim cron（第 13 章の蓄積子）、オラクル publisher（Pyth を使わず自前で走らせるなら第 9 章の `SetOraclePrice`）、マッチング エンジン cranker（非同期マッチングを組み立てていたら）、フロントエンドにフィードするオフチェーン インデクサ。章は新しいオンチェーン コードがゼロ — その内容はオフチェーン プロセスの設計パターン、手数料経済、冗長性とフェイルオーバー、「Solana DEX」がオンチェーン プログラム半分とコーディネートされたオフチェーン インフラ半分であるというアーキテクチャ上の現実だ。
