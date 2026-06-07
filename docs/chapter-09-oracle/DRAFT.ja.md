# 第9章 — オラクル取り込み: Pyth 内部

> 状態: ドラフト (v0.1)。
> 教材コード: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs)（`Oracle`、加えて §9.5 のための Pyth v1 と v2 のレイアウト定数）、[`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（`process_create_oracle`、`process_set_oracle_price`、`process_place_order_checked`、`process_place_order_checked_pyth`、`process_place_order_checked_pyth_v2`、ヘルパ `read_fresh_oracle` / `read_fresh_pyth_v1_oracle` / `read_fresh_pyth_v2_oracle`）、[`scripts/oracle/src/main.rs`](../../scripts/oracle/src/main.rs)。
> 参照対象: Pyth Network mainnet プログラム（`FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH`）、Switchboard On-Demand。

---

## §9.0  はじめに — 意図的なモック

外部価格オラクルのない perp DEX とは、「最後の取引が起きたところ」がそのままマーク価格になるデリバティブ市場のことだ。古びた板や薄い瞬間が最終取引を spot 市場から引き離すまで、それで動く — その時点で清算エンジンが現実と無関係な価格でトリガし始め、保険基金がそれを払う。マーク価格はファンディングレート、清算、必要証拠金、その他あらゆるリスク側計算の load-bearing な入力だ。プログラム自身のトレードテープから来てはならない。外部から来なければならない。

Solana 上の標準的な答えは **Pyth Network**（よく副として **Switchboard**）。両方とも資産ごとの価格アカウントを公開しており、任意のプログラムが読める。アカウントは publisher のプログラム所有で、あなたの所有ではない — あなたは厳密に読み手だ。

本章は、読み手がオラクル入力を安全に扱う方法についての章だ。仕事は 3 つに分かれる。

1. 価格アカウントを見つけ、レイアウトを検証し、price + confidence + exponent を読む。
2. Clock sysvar で鮮度をチェックし、古い価格での操作を拒否する。
3. 価格を意味あるプログラムチェックに適用する — ここでは `place_order` のサニティバンド。

教材例として、Pyth 価格フィードと同じ形をした独自の `Oracle` アカウント型を組み立て、それを自前のプログラムが所有する。これは意図的なスコープ判断だ。真の Pyth 統合なら `pyth-sdk-solana` をインポートし、価格更新アカウントの所有者チェックを `pyth_program_id` から取り、自明でない v2 形式の価格更新メッセージをパースすることになる。それをここでやれば、**読み取りパターン**ではなく SDK 呼び出しを教えることになる。オラクルをローカル所有にすることで publish 瞬間を制御でき、staleness 実験が容易になる — そして手法（staleness チェック、サニティバンド、防御的パース）はそのまま転用できる。本章は本番との差を慎重に明示する。

---

## §9.1  形だけの Pyth、要約

本物の Pyth v1 価格アカウントは約 3 KiB の構造体で、小さなヘッダ（magic + version + type + size）、製品紐付け、最近の価格観測の配列を含む。実際に必要なフィールドは 24 バイトに収まる。

```text
price:        i64    // 符号付きマンティッサ
conf:         u64    // 1σ confidence interval、同単位
expo:         i32    // 10 進指数（通常負、例 -8 → 8 桁小数）
publish_slot: u64    // この価格が最後に更新されたスロット
```

本物のマーク価格は `price × 10^expo`。confidence interval `conf × 10^expo` は価格がどれだけ tight かを表す — conf が広ければ publisher が不確かを意味し、多くのプログラムは `conf > tolerance × price` の価格を拒否する。

`crates/state/src/lib.rs` の本書の `Oracle` 構造体はちょうどこの形に、discriminator、bump、価格対象 market を加えたものだ。

```rust
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Oracle {
    pub discriminator: [u8; 8],   // 0..8   — ORACLE\0\0
    pub bump: u8,                  // 8
    pub _pad0: [u8; 7],            // 9..16
    pub market: [u8; 32],          // 16..48
    pub price: i64,                // 48..56
    pub conf: u64,                 // 56..64
    pub expo: i32,                 // 64..68
    pub _pad1: [u8; 4],            // 68..72
    pub publish_slot: u64,         // 72..80
    pub _reserved: [u8; 32],       // 80..112
}
```

合計 112 バイト、Pod、repr(C)。第 7 章の OrderBook と同じ手で、生のアカウントデータから bytemuck キャストで割り当てなしに取れる。

本物の Pyth 統合との違い、驚かないように明示しておく。

| 観点 | 本書の `Oracle` | 本物の Pyth |
|---|---|---|
| アカウント所有者 | openhl-core（本書のプログラム） | Pyth プログラム（mainnet では `FsJ3...epH`） |
| 所有者チェック対象 | `program_id`（自前） | `&pyth_program::ID` |
| アカウントレイアウト | この 112 バイト構造体 | Pyth v1 PriceAccount（約 3 KiB）または v2 更新メッセージ |
| 更新の仕組み | `SetOraclePrice` 命令（本書の publisher） | Pyth publisher が Pyth プログラムを呼ぶ |
| Discriminator | `ORACLE\0\0`（本書の慣習） | Pyth の magic 定数 + version フィールド |
| 古さの時計 | Clock sysvar `slot`（本書の publish_slot） | Pyth の `publish_time` + `prev_publish_time` |

右列のすべての項目に、左列の直接対応物がある。本書の `Oracle` でやることはすべて、本物の Pyth アカウントでもやる。magic 定数と所有者チェックが違うだけだ。

> **演習 §9.1.** mainnet の Pyth SOL/USD 価格アカウントを引け。そのサイズ（バイト）、所有プログラム、data の最初の 4 バイト（Pyth magic 定数）を確認せよ。本書の `Oracle` のサイズ、所有者、最初の 8 バイトと比較せよ。

---

## §9.2  オラクルを書く — `SetOraclePrice`

章で staleness シナリオを試すには、既知の瞬間にオラクルを書く手段が要る。`programs/openhl-core/src/lib.rs:1513–1571` から。

```rust
fn process_set_oracle_price(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    // ... ペイロード + アカウント検証 ...
    msg!("set_oracle_price: open-auth publisher = {}", publisher_ai.key);

    let clock = Clock::get()?;
    let mut data = oracle_ai.try_borrow_mut_data()?;
    let oracle: &mut Oracle = bytemuck::from_bytes_mut(&mut data[..Oracle::LEN]);
    // ... discriminator チェック ...

    oracle.price = price;
    oracle.conf = conf;
    oracle.expo = expo;
    oracle.publish_slot = clock.slot;
    // ...
}
```

吸収すべき点は 2 つ。

**`Clock::get()?` は syscall だ。** Clock sysvar は `slot`、`epoch`、`unix_timestamp`、その他いくつかのフィールドを持つ。プログラムが現在実行中のスロットを知る**唯一の**手段だ。プログラムは壁掛け時計を読めず、ユーザが供給する現在時刻も信用できない。書き込み時に `publish_slot = clock.slot` を刻むのが、読み取り時の staleness チェックの基盤になる。

**publisher チェックは意図的に欠落している。** 命令は signer を受け入れるが、**どの**signer かは検証しない。本番ではこれは誤りだ — プログラム ID を持つ誰もが任意の価格を書け、サニティバンドに任意の limit を受け入れさせられる。修正は次のとおり。

1. **既知の publisher pubkey にピン留め。** `pub const ORACLE_PUBLISHER: Pubkey = ...;` を読み、`publisher_ai.key == &ORACLE_PUBLISHER` をチェックする。単純、pubkey が変わるなら rotation 手続きが必要。
2. **マルチ publisher 署名。** 受け入れ可能な publisher 集合をオラクルアカウント自身に格納。いずれの signer が一致すれば良い。
3. **アカウントを Pyth に渡す。** オラクルアカウントを Pyth プログラム所有にして、`SetOraclePrice` を完全に取り除く。これでオラクルは自前で書けなくなる、本番として正しいアーキテクチャだ。

章は (1) と (2) を演習として、(3) を散文で歩く。意図的な auth ギャップは、読者が §9.3 の staleness シナリオで既知のスロットで価格を止められるようにするためだ。

> **演習 §9.2.** `programs/openhl-core/src/lib.rs` に `ORACLE_PUBLISHER: Pubkey` 定数と、`process_set_oracle_price` に明示的な `publisher_ai.key == &ORACLE_PUBLISHER` チェックを追加せよ。定数は自分のウォレット pubkey に。`oracle --set ...` が自分のウォレットから動くが、新規鍵ペアから失敗することを確認せよ。

---

## §9.3  オラクルを読む — 基礎チェックとしての staleness

読み手パターンは小さなヘルパ `read_fresh_oracle`（lib.rs:1965–1987）に住み、`process_place_order_checked` から呼ばれる。第 11 章で追加する 3 つのポジションハンドラからも再利用するため、ガントレットは 4 度コピーされる代わりに 1 箇所に保たれる:

```rust
fn read_fresh_oracle(
    oracle_ai: &AccountInfo,
    program_id: &Pubkey,
) -> Result<u64, ProgramError> {
    if oracle_ai.owner != program_id || oracle_ai.data_len() != Oracle::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    let oracle_data = oracle_ai.try_borrow_data()?;
    let oracle: &Oracle = bytemuck::from_bytes(&oracle_data[..Oracle::LEN]);
    if oracle.discriminator != ORACLE_DISCRIMINATOR {
        return Err(ProgramError::UninitializedAccount);
    }
    if oracle.price <= 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    let clock = Clock::get()?;
    let age = clock.slot.saturating_sub(oracle.publish_slot);
    if age > MAX_ORACLE_STALENESS_SLOTS {
        msg!("oracle stale ({} slots, max {})", age, MAX_ORACLE_STALENESS_SLOTS);
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(oracle.price as u64)
}
```

`process_place_order_checked` はこれを `let mark = read_fresh_oracle(oracle_ai, program_id)?;` として呼び、`mark` に対してサニティバンドを走らせる。

価格を信用する前に 4 つのチェック。

1. **Discriminator チェック**（`oracle.discriminator != ORACLE_DISCRIMINATOR`）: 初期化されていないオラクルアカウントを拒否する。本物の Pyth では magic 定数 + version 一致がこれにあたる。
2. **価格正値チェック**（`oracle.price <= 0`）: 非正価格のオラクル状態を拒否する。本物の Pyth は時折 `0` を「今は良い価格がない」シグナルとして publish する — 読み手はそれを扱わねばならない。
3. **Staleness チェック**（`age > MAX_ORACLE_STALENESS_SLOTS`）: 25 slot（約 10 秒）より古い価格を拒否する。これが本章の中心だ。鮮度をチェックできない価格は信用できない価格だ — publisher を止められる攻撃者（あるいは単にネットワーク障害を利用する者）が、それを盲信するプログラムを古い価格でゲームできるからだ。
4. **所有者チェック**（1969 行、`oracle_ai.owner != program_id`）: 異なるプログラム由来のアカウントを拒否する。本物の Pyth では `oracle_ai.owner == &pyth_program::ID`。

`lib.rs:205` の `MAX_ORACLE_STALENESS_SLOTS = 25`。選び方はワークロードによる: 25 slot は現行ターゲットスロット時間で約 10 秒。高ボラペア（BTC、ETH の荒れた日）なら、もっと tight に — おそらく 10〜15 slot。ステーブルコインペアならもっと wide で許せる。定数は理想的には market ごとに調整できるよう `Market` 構造体に持つべきだが、本書は簡潔さのためグローバルに置く。

`read_fresh_oracle` 内の借用は自然にヘルパ本体にスコープされる — 関数が返ったときにドロップし、呼び出し側が book を変更する前に解放される。これが重要なのは、オラクルと book の両方が `AccountInfo` として渡され、ランタイムは同じアカウントメモリの 2 つの可変借用が共存しないことを要求するからだ。本書のオラクルと book は別アカウントだとしても、借用をタイトにスコープするパターンは良い衛生だ — ハンドラが大きくなったときの微妙な aliasing バグを防ぐ。

**SDK が隠していること:** `pyth-sdk-solana::load_price_feed_from_account_info` は discriminator チェック、所有者チェック、型付き `PriceFeed` へのデシリアライズを行う。staleness チェックは**行わない** — それは常にあなたの仕事だ。明示的な staleness ゲートなしに Pyth を使うプログラムは、DeFi における最大のオラクルバグ群の 1 つに属して出荷される。

> **演習 §9.3.** スロット N でオラクル価格を設定せよ（`oracle --set --price 100 ...` を実行し、出力からスロットを控える）。30 slot 待つ（約 12 秒、`solana confirm` を適当な tx に当てればスロットがわかる）。フラグなしで `oracle` を実行する。`age (slots)` が 25 を超えるはずだ。`place-order-checked` を実行する（配線しているとして）— `oracle stale` で失敗するはずだ。

---

## §9.4  オラクルを使う — サニティバンド

staleness チェック済みの価格はもう安全に読める。最初のリスク制御として使うこと: オラクル mark から大きく外れる limit 価格の `place_order` 呼び出しを拒否する。

`process_place_order_checked` 1658–1671 行。

```rust
let band = mark.saturating_mul(SANITY_BAND_BPS) / 10_000;
let low = mark.saturating_sub(band);
let high = mark.saturating_add(band);
if price < low || price > high {
    msg!(
        "place_order_checked: price {} outside sanity band [{}, {}] (mark={})",
        price,
        low,
        high,
        mark
    );
    return Err(ProgramError::InvalidArgument);
}
```

`SANITY_BAND_BPS = 2000`（lib.rs:211）で ±20%。`mark = 100` なら、価格 50 の注文は拒否される（`low = 80` 未満）、95 の注文は受け入れ、121 の注文は拒否。意図的に wide なバンド: tight なバンドは通常ボラの間に正当ユーザを失敗させる頻度を上げるし、章は**パターン**についての章であって calibration ではない。

本番バンドは market ごとに調整される。

- **高ボラ perp（荒れた日の BTC、ETH）:** 5〜10% が受け入れ可能かも。これより wide だと正当な fat-finger 隣接注文を拒否しすぎる。
- **中ボラ perp（SOL、AVAX）:** 3〜5% 典型。
- **低ボラペア（ステーブルコイン perp、FX）:** 1〜2%、ときにそれよりも tight。

バンドが最初のリスク制御である理由は、外部真実に依存する最も単純なものだから。ファンディングレート（第 10 章）と清算（第 11 章）は同じオラクル読み取りの上に立ち、より難しい数学に適用する。

**Saturating 算術。** `saturating_mul` と `saturating_sub` は意図的だ。`mark = u64::MAX`（実務上不可能だが理論上）だと乗算は wrap する。Saturating はそれを `[u64::MAX - band, u64::MAX]` のバンドに縮め、wraparound で 0..何かのバンドが生まれる代わりに、すべての合理的注文を優雅に失敗させる。Solana のプログラム ランタイムは整数オーバーフローでパニックする（`release` ビルドでは静かに wrap、`debug` ではパニック）— 明示的な saturating 演算は監査で報われる小さな習慣だ。

> **演習 §9.4.** オラクル価格を 100 に設定。価格 90（バンド内）、75（バンド外 — low 80 未満）、120（ぎりぎり内 — mark*0.2=20 なので high は 120）で注文を試す。各々を辿る。次に `SANITY_BAND_BPS` を 500（5%）に変えて同じ価格で再テスト。

---

## §9.5  本番 Pyth v1 — bytes-up で実装

モック `Oracle` は章が既知のスロットでパブリッシュを止められるように作られている。本番では、パブリッシャが独立に populate する Pyth アカウントから読む。マイグレーションは局所的 — 同じ staleness ガントレット、同じサニティバンド、違うパーサ — で、本章はそれを peer 命令 `PlaceOrderCheckedPyth`（タグ 32）として出荷し、2 つのパスを並べて比較できるようにする。Pyth v1 のレイアウトは bytes-up でパースできるほどよく定義されている — `pyth-sdk-solana` 依存なし。第 1 章の bytes-up 姿勢がそのまま通る。

### Pyth v1 PriceAccount のレイアウト

すべての Pyth v1 price アカウントはマジック定数 + バージョン + アカウント型識別子で始まり、EMA 簿記のヘッダ、最後に本当に信用する集約 `agg` ブロックがある。本書が読む 6 フィールド:

```text
offset | size | field                       | チェックする内容
-------+------+-----------------------------+---------------------------------
   0   |  4   | magic (u32)                 | == 0xa1b2c3d4
   4   |  4   | ver   (u32)                 | == 2 (Pyth v1)
   8   |  4   | atype (u32)                 | == 3 (PriceAccount)
  20   |  4   | expo  (i32)                 | ログのみ; 適用しない（下記参照）
 208   |  8   | agg.price (i64)             | > 0
 216   |  8   | agg.conf  (u64)             | ログ
 224   |  4   | agg.status (u32)            | == 1 (Trading)
 232   |  8   | agg.pub_slot (u64)          | clock.slot - pub_slot <= MAX_ORACLE_STALENESS_SLOTS
```

（`agg` はそのスロットでのパブリッシャネットワークの集約価格。Pyth は最大 32 パブリッシャを持ち、オンチェーン集約は毎スロット走る; `agg.status` がそれらが同意したかを反映する。）

オフセットは `crates/state/src/lib.rs` から `PYTH_V1_OFFSET_*` 定数としてエクスポートされる。完全な ~3 KiB の `PriceAccount` を Pod 構造体としてミラーするのは意図的に避ける — バイトの大半は本書が読まない publisher ごとの `comp[]` エントリだ。6 オフセットをハードコードするのはコードバイトが少なく、監査しやすく、本章の「バイトレイアウトが**契約**」のスタンスに一致する。

### `read_fresh_pyth_v1_oracle`

ヘルパ、骨格で:

```rust
fn read_fresh_pyth_v1_oracle(oracle_ai: &AccountInfo) -> Result<u64, ProgramError> {
    let data = oracle_ai.try_borrow_data()?;
    if data.len() < PYTH_V1_MIN_LEN { return Err(InvalidAccountData); }

    // (1) ヘッダ ガントレット。
    if u32::from_le_bytes(data[0..4]) != PYTH_V1_MAGIC      { return Err(...); }
    if u32::from_le_bytes(data[4..8]) != PYTH_V1_VERSION    { return Err(...); }
    if u32::from_le_bytes(data[8..12]) != PYTH_V1_ACCOUNT_TYPE_PRICE { return Err(...); }

    // (2) 集約ステータス。
    let status = u32::from_le_bytes(data[PYTH_V1_OFFSET_AGG_STATUS..]);
    if status != PYTH_V1_STATUS_TRADING { return Err(InvalidAccountData); }

    // (3) Staleness — agg.pub_slot vs Clock.slot。
    let pub_slot = u64::from_le_bytes(data[PYTH_V1_OFFSET_AGG_PUB_SLOT..]);
    let age = Clock::get()?.slot.saturating_sub(pub_slot);
    if age > MAX_ORACLE_STALENESS_SLOTS { return Err(InvalidAccountData); }

    // (4) 価格を読み、観測性のため expo/conf をログ。
    let price_i = i64::from_le_bytes(data[PYTH_V1_OFFSET_AGG_PRICE..]);
    if price_i <= 0 { return Err(InvalidAccountData); }
    Ok(price_i as u64)
}
```

チェック順序はモックパスをミラーする: 構造検証が先（magic / version / atype）、次にステータスチェック（本書モックの discriminator+initialization チェックの Pyth 類似）、次に staleness、最後に価格サニティ。どの失敗も `InvalidAccountData` を返すので、呼び出し側は stale なまたは非 Trading な Pyth フィードをモックの stale 処理と同じように扱える。

フラグすべき設計判断 4 つ:

**1. Pyth アカウントに所有者チェックなし。** モックパスは `oracle_ai.owner == program_id` を要求するが、ここでは意図的に `oracle_ai.owner == &pyth_program::ID` を**要求しない**。Pyth プログラム ID をハードコードすると mainnet 対 devnet の選択を最初から強いる（mainnet は `FsJ3...epH`、devnet は `gSbE...92s`）、加えて magic+version+atype ガントレットがすでにアカウントが Pyth v1 PriceAccount であることを構造的に保証する。本番デプロイでは (a) 期待される Pyth プログラム ID を `Market` 構造体に固定するか、(b) 期待される Pyth アカウントの**pubkey** を `Market` に固定する — 選択 (b) は呼び出し側が間違った資産の Pyth フィードを差し替えるのを防ぐので、より tight だ。

**2. `expo` は読みログするが適用しない。** Pyth 価格は `(mantissa, expo)` ペアで来る — 本物の価格は `mantissa × 10^expo`。USD ペアでは `expo` は典型的に `-8`。モック側に expo はない（本書 `Oracle.price` は market が使うスケールでの u64）ので、本章で 2 パスを相互運用可能に保つため、mantissa を価格として扱い `expo` を観測性のためにログする。本番デプロイは正規化する: 単一サポート `expo` を固定し他を拒否するか、その場でプログラムの固定小数点規約に翻訳するか。これがマイグレーションで本物のバグを書く可能性が最も高い場所; 本章はそれを呼び出して TODO を自分で置けるようにする。

**3. status == Trading でも `price <= 0` は拒否する。** Pyth は時々 0 を publish する — 集約が非負だが degenerate な publisher ネットワークのコーナーケース。モックパスも同じく拒否する; Pyth パスも合わせる。

**4. `agg.status != Trading` はハード拒否、フォールバックではない。** Switchboard フォールバック付きの本物の本番リーダーは、ここでエラーを返さずに他のオラクルを試すだろう。本章はフォールバックを出荷しない、教材例が v1 単一オラクルマイグレーションだからだ; フォールバックパターン（try-A-then-B 不一致許容付き）は上に乗せるのは簡単 — §9.5 の closing ノートが配置する。

### `PlaceOrderCheckedPyth`（タグ 32）

ピア命令。`PlaceOrderChecked` と同じペイロード（`[side u8][price u64 LE][size u64 LE]`）、同じ 8 先頭アカウント、2 変更を除く:

- スロット 2 は Pyth v1 PriceAccount、モック `Oracle` ではない（market から oracle PDA クロスチェックはドロップ; Pyth アカウントは PDA 導出対象ではない）。
- オラクル側ガントレットは `read_fresh_oracle` ではなく `read_fresh_pyth_v1_oracle` を呼ぶ。

下流すべて — サニティバンド、手数料エスクロー、book write — はモックパスと同一。両ハンドラは同じ `SANITY_BAND_BPS`、`PROTOCOL_FEE_BPS`、末尾の同じ `spl_token_transfer_user_signed` Transfer を共有する。マイグレーションの surface は §9.5 が約束したとおり: 局所的。

### Pyth v2 — 同じ章、違うアーキテクチャ

`read_fresh_pyth_v1_oracle` の「v1」が重要。Pyth の v1 モデル — パブリッシャが毎スロット静的 PriceAccount に書き、リーダーがそれをパース — は廃止されつつあり、v2 の **プルオラクル** アーキテクチャに置き換わる。Perp DEX にとって重要な洞察は、v2 アーキテクチャが 2 つの層にきれいに分かれることだ:

1. **Pyth の `pyth-solana-receiver` プログラム**が暗号作業をする。Wormhole VAA（Verified Action Approval — Wormhole の guardian set の 2/3 以上が署名したペイロード）に包まれた Pyth update メッセージを受け取り、Solana の secp256k1 precompile で guardian 署名を検証し、bundled update の Merkle proof があれば検証し、検証済み価格を `PriceUpdateV2` アカウントに書く。
2. **コンシューマ**（あなた、perp DEX）がその `PriceUpdateV2` アカウントを読む。VAA 検証なし。Guardian set 管理なし。あなたのホット パスに暗号なし。

その 2 層を混同したら（本セクションの初期ドラフトはした）、v2 はあなたの側に ~2000 行の暗号コードが必要だと結論するだろう。そうではない。Wormhole 作業は Pyth のプログラムに住み、それは共有インフラだ; perp DEX で出荷するのは構造的に v1 リーダーと同じ、ただし異なるアカウント レイアウトに対するもの。

`PlaceOrderCheckedPythV2`（タグ 33）がコンシューマ側を出荷する。リーダー:

```rust
fn read_fresh_pyth_v2_oracle(oracle_ai: &AccountInfo) -> Result<u64, ProgramError> {
    let data = oracle_ai.try_borrow_data()?;
    if data.len() < PYTH_V2_MIN_LEN { return Err(InvalidAccountData); }

    // (1) Anchor account discriminator。PriceUpdateV2 は Anchor の
    // sha256("account:PriceUpdateV2")[..8] 規約を使う。
    if data[0..8] != PYTH_V2_PRICE_UPDATE_DISCRIMINATOR { return Err(...); }

    // (2) VerificationLevel — ログのみ。本番は Full を要求するかも。
    let level_tag = data[PYTH_V2_OFFSET_VERIFICATION_LEVEL];
    // Partial { num_signatures }  | Full

    // (3) Staleness — posted_slot に対して、receiver が update を書いた
    // Solana slot。
    let posted_slot = u64::from_le_bytes(data[PYTH_V2_OFFSET_POSTED_SLOT..]);
    if Clock::get()?.slot.saturating_sub(posted_slot) > MAX_ORACLE_STALENESS_SLOTS {
        return Err(InvalidAccountData);
    }

    // (4) 価格 + exponent / conf / publish_time をログ。
    let price_i = i64::from_le_bytes(data[PYTH_V2_OFFSET_PRICE..]);
    if price_i <= 0 { return Err(InvalidAccountData); }
    Ok(price_i as u64)
}
```

PriceUpdateV2 のオンチェーン レイアウト、オフセットでパース（Anchor が Pyth の構造体を borsh シリアライズする）:

```text
offset | size | field                            | 何に使うか
-------+------+----------------------------------+---------------------------------------
   0   |  8   | anchor_discriminator             | == [34,241,35,99,157,126,244,205]
   8   | 32   | write_authority (Pubkey)         | （無視 — receiver の署名 PDA）
  40   |  1   | verification_level tag           | ログ; 0=Partial、1=Full
  41   |  1   | verification_level.num_signatures| ログ（tag==Partial のときのみ）
  42   | 32   | price_message.feed_id            | （呼び出し側がオフチェーンで期待 feed を固定）
  74   |  8   | price_message.price (i64)        | > 0; u64 mark として返す
  82   |  8   | price_message.conf  (u64)        | ログ
  90   |  4   | price_message.exponent (i32)     | ログ; 本番は 10^expo で正規化
  94   |  8   | price_message.publish_time (i64) | ログ; 情報用（Pyth の Unix timestamp）
 102   |  8   | price_message.prev_publish_time  | 未使用
 110   |  8   | price_message.ema_price          | 未使用（章は spot 価格を使う）
 118   |  8   | price_message.ema_conf           | 未使用
 126   |  8   | posted_slot (u64)                | Staleness ガントレット
 134                                                — 終わり
```

オフセットは `crates/state/src/lib.rs` から `PYTH_V2_OFFSET_*` としてエクスポートされる。v1 と同様、完全な Pod 構造体をミラーしない — 6 フィールド読み込み、Anchor 依存なし、バイト レイアウトが**契約**だ。

### 2 命令 トランザクション パターン

v2 リーダーが前提とするアーキテクチャの一片: トレード命令が走るとき `PriceUpdateV2` アカウントが*fresh*でなければならない。実際にはコンシューマ トランザクションは receiver の `post_update_v2` 命令を最初の命令として、トレード命令を 2 番目として含めることでこれを行う:

```text
tx = [
  pyth-solana-receiver::post_update_v2(vaa_data, update_data, …),  // VAA 検証、アカウント書き込み
  openhl::place_order_checked_pyth_v2(side, price, size),          // 今書かれたアカウントを読む
]
```

`post_update_v2` は per-(feed_id, write_authority) `PriceUpdateV2` PDA を書き、`place_order_checked_pyth_v2` がそれを読む。クライアントは Pyth の price-update service（パブリッシャ署名を VAA に集約する HTTP エンドポイント）から最新 VAA を fetch し、最初の命令にバンドルし、staleness ガントレットがすべてを同期させる。本書のハンドラから receiver へのオンチェーン CPI はない: アカウント経由で通信し、直接ではない。

これはまた、本書の v2 ハンドラが「post」命令や Wormhole アカウントを取らない理由でもある: コンシューマ トランザクションが配線、プログラムは単に読む。

### 真に繰り延べられているもの

コンシューマ側が出荷された今、残るのは**自前の Wormhole スタイル receiver を走らせること**だ。Wormhole チェーンに公式 Pyth receiver がないところから Pyth updates を読みたい場合、あるいは receiver が公開しない追加メッセージ型（ガバナンス updates、mapping changes）を検証したい場合に有用だ。それ自体が独自章だ — 保存された guardian set に対する secp256k1 署名復元、リプレイ保護、bundled updates の Merkle proof 検証。ここの perp DEX コンシューマ パスはそのどれも必要としない。

### Switchboard フォールバック

単一オラクルは単一障害点。Pyth は停止したことがある。Switchboard も停止したことがある。両方同時に（稀に）起きたこともある。下方を守るプログラムは**両方**を信頼し、どちらも fresh でなければ動作を拒否する。配線は機械的だ:

1. トランザクションの `AccountMeta` 配列に両オラクルアカウントを含める。
2. ハンドラがそれぞれを読み、完全な検証パターン（discriminator/magic + status + 価格正値 + staleness）を行う。
3. どちらかが通れば使う。両方失敗なら呼び出し拒否。

これを行うプログラムは典型的に、両方が fresh のときは 2 つを**比較**もする — ある許容（例: 50 bp）を超えて不一致なら呼び出し拒否。Pyth と Switchboard の 50 bp 不一致は通常どちらかが誤っており、ユーザに安い方を選ぶだけのプログラムはゲームされてきた。

> **演習 §9.5.** 本物の Pyth v1 mainnet PriceAccount を選べ（SOL/USD は執筆時点で `H6ARHf6YXhGYeQfUzQNGk6rDNnLBQKrenN712K4AQJEG` — Pyth の price-feed-ids ページで検証せよ）。ローカル validator の `clone` 機能を使ってそのアカウントをテストクラスタにミラーし、それに対して `PlaceOrderCheckedPyth` を invoke せよ。ログされた `expo` を SOL/USD product と比較せよ（`-8` のはず）。Staleness ガントレットをアカウントの古いスロットをアーカイブから再生して確認せよ。

---

## §9.6  まとめと自己検証

### まとめ図

```
外部真実 → プログラム安全:

  Pyth/Switchboard publisher          本書の Oracle アカウント
   ┌──────────────────┐                ┌───────────────────────┐
   │ 数 slot ごとに    │ ── 所有 ────► │  Pyth プログラム (mainnet) │
   │ price+conf を書く │                │  openhl-core (本書)    │
   └──────────────────┘                └───────────────────────┘
                                                │  読む
                                                ▼
                                   ┌─────────────────────────────┐
                                   │ place_order_checked         │
                                   │  - オラクル アカウント ロード │
                                   │  - 所有者チェック             │
                                   │  - discriminator チェック    │
                                   │  - price > 0                │
                                   │  - Clock 経由の staleness   │
                                   │  - ±X bps のサニティバンド   │
                                   │  - 通常の place を走らせる    │
                                   └─────────────────────────────┘

価格を信用する前に必要なチェック:

  ┌────────────────────────┬──────────────────────────────────┐
  │ チェック               │ どこで                            │
  ├────────────────────────┼──────────────────────────────────┤
  │ owner == オラクル プログラム │ どのデータ読みより先          │
  │ discriminator 一致      │ data の最初の 8 バイト           │
  │ price > 0              │ 「価格なし」シグナルを拒否        │
  │ slot age <= MAX        │ Clock sysvar を要する             │
  │ （任意）conf 小         │ 広い / 不確かな価格を拒否        │
  │ サニティバンド          │ 価格をユーザ入力に適用            │
  └────────────────────────┴──────────────────────────────────┘
```

### 自分で検証する 3 項目

1. **Discriminator チェックは重要。** market PDA を作り、`place_order_checked` のオラクルスロットに market PDA を渡すトランザクションを組み立てよ。`read_fresh_oracle` の [`lib.rs:1974`](../../programs/openhl-core/src/lib.rs#L1974) の discriminator チェックが `UninitializedAccount` で失敗するはずだ。このチェックがないと、コードは `bytemuck::from_bytes` でゴミデータに当てて意味のない `price` を使う。
2. **Staleness はセキュリティゲート。** オラクルを設定し、30+ slot 待ち、バンド内の任意の価格で注文を試みよ。`oracle stale` で失敗するはずだ。これが最も忘れられがちなチェックで、実戦で最多のオラクル exploit を生んできたチェックでもある。
3. **バンドの境界は厳密。** `SANITY_BAND_BPS = 2000`、`mark = 100` で、ちょうど 80 の注文は**受け入れ**られるはずだ（チェックは `< low`、`<= low` ではない）。ちょうど 79 の注文は拒否されるはずだ。両方を実行して確認せよ。`<=` と `<` の slip エッジケースは 1 bp、tight なバンドや高価格では差のドル額が無視できなくなる。

---

## 第 10 章への導線

マーク価格を手にした。perp DEX がそのマークでやる次のことは**ファンディングレート**だ。ファンディングは、ロングとショートのポジションが perp の価格を spot に係留するために定期的に支払いを交換する仕組み — 形式的には `funding_rate ≈ k × (perp_premium_index - mark_price) / mark_price`、`perp_premium_index` は最近の約定価格に対する何らかの蓄積、`mark_price` はちょうどいま読み方を学んだもの。レートはファンディング窓口ごとに支払う（多くの取引所で 1 時間、一部で 8 時間）、プログラムは無制限ループなしにポジションごとの決済を継続的に蓄積しなければならない。

第 10 章では、時間窓蓄積パターン、ファンディング期限のための Clock sysvar の `unix_timestamp` フィールド、すべてのポジションに「このスロットでファンディング支払い」を単一トランザクションが全ポジションに触れずに行わせるためのクランク/keeper パターンを歩く。ここが、Phase A の並列性レッスン（第 5 章）がデータレイアウトを支配し始める場所だ: ファンディング決済はシングルトンの「合計」アカウントがボトルネックになる典型ケースで、§5.5 のシャーディングパターンを使ってそれを避ける。
