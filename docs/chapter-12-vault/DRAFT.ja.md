# 第12章 — ネイティブ Vault プログラム（プール取引）

> 状態: ドラフト (v0.1)。
> 教材コード: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs)（`TradingVault`、`VaultShare`）、[`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（`process_create_trading_vault` 2195–2280 行、`process_vault_deposit` 2282–2413 行、`process_vault_withdraw` 2415–2500 行、`process_vault_update_nav` 2502–2544 行）、[`scripts/vault/src/main.rs`](../../scripts/vault/src/main.rs)。

---

## §12.0  はじめに — 名前についての明確化

「vault」という単語はこのコードベースに 2 回登場する。

- **第 6 章の `Vault`** は SPL Token アカウントだった — 単一ユーザの担保が住む単一ユーザ用カストディ アカウント。所有者: SPL Token プログラム。純粋な配管。
- **第 12 章の `TradingVault`** はまったく別物 — 複数ユーザが資産を預け、shares 経由でマネージャの PnL を pro-rata で共有するプール基金。所有者: openhl-core。shares、NAV、deposit/withdraw 経済を持つ。

両方とも「vault」の合理的な使い方だ。型名で曖昧さを解消した（`TradingVault` は明確、第 6 章の `Vault` は専用 state 構造体を持たず — 単なる SPL Token アカウントだ）。命令は `CreateVault`（トークン アカウント）vs `CreateTradingVault`（本章）の分離で衝突しない。

trading vault は perp DEX を、ユーザが直接取引する venue から、**基金**もホストする venue に変える概念的プリミティブだ。ポジションを自分で管理したくないユーザは vault に預金できる。vault のマネージャが戦略を走らせる。預金者はリターンを共有する。これは Solana 上のすべての yield vault の構造 — Drift の spot vault、Kamino の leveraged vault、Jupiter の perps vault、その他もろもろ。

本章は vault の会計半分を組み立てる — shares、deposits、withdrawals、NAV 更新。マネージャの取引半分は組み立てない（第 11 章の `OpenPosition` を vault の PDA をポジション所有者として呼ぶ薄いラッパ命令になる）。share 会計が整えば追加は機械的だ。本章は設計を説明し、実装を宿題ピースとして残す。第 11 章が SPL Token エスクローを後送りしたのと同じやり方だ。

本章が実際に扱うこと:

1. **share/asset 数学** — NAV が変わるとき deposit と withdrawal が pro-rata 不変条件をどう保つか。
2. **シングルトン書き込みの再起** — すべての deposit と withdrawal が同じ `TradingVault` アカウントを変更するので、vault 操作はスケジューラで直列化する。第 5 章のアンチパターンを**意図的に**導入したので、緩和策が実務でどう見えるかが見える。
3. **マネージャ信用モデル** — `VaultUpdateNAV` が肝心の信用前提。それがどう構造化されるかが、vault が「マネージャが嘘をつかないと信じる」か「NAV をオンチェーン状態に対して検証する」かを決める。

---

## §12.1  2 つのアカウント型

`crates/state/src/lib.rs` から。新規構造体 2 つ、両方 Pod、repr(C)。

**`TradingVault`**（160 バイト）: (market, manager) ペアあたり 1 つ。

```rust
pub struct TradingVault {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub market: [u8; 32],
    pub manager: [u8; 32],   // トレーダ; UpdateNAV に署名する
    pub mint: [u8; 32],      // 資産 mint（例: USDC）
    pub total_shares: u64,
    pub total_assets: u64,   // NAV — マネージャ報告値
    pub _reserved: [u8; 32],
}
```

load-bearing なフィールドは `total_shares` と `total_assets` の 2 つ。その比が share あたり NAV。すべての deposit と withdrawal で**一緒に**更新（比例的に、share あたり値を保つ）、マネージャからの NAV 更新では**独立に**更新される。

`market` と `mint` は非正規化 — (market, manager) ペアはすでに PDA のシード スキームでこれらを含意するが、アカウント内に保存することで読み手が外部コンテキストから再派生せずに vault を識別できる。`manager` は `VaultUpdateNAV` が signer と照合するものだ。

**`VaultShare`**（128 バイト）: (vault, depositor) ペアあたり 1 つ。

```rust
pub struct VaultShare {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub vault: [u8; 32],
    pub owner: [u8; 32],
    pub shares: u64,
    pub cost_basis: u64,     // 累積預金額（報告用）
    pub _reserved: [u8; 32],
}
```

`shares` は預金者の share 数。`cost_basis` は彼らが預けた quote 資産の累積額 — P&L 報告に使う（gain = `withdraw_assets - cost_basis_share`）、プログラム内ロジックには使わない。預金者は所有を証明するため `VaultWithdraw` に署名する。

PDA 派生:

- TradingVault: `[b"trading_vault", market.key, manager.key]`
- VaultShare: `[b"vault_share", vault.key, owner.key]`

> **演習 §12.1.** あるユーザが vault の 200 shares を保有、`total_shares = 1000`、`total_assets = 1500`。vault のどれだけを所有しているか、share あたり NAV は? マネージャが成功取引を走らせ `total_assets` を（`total_shares` は変えずに）1800 に押し上げたとき、新しい share あたり NAV は?

---

## §12.2  share/asset 数学

操作 3 つ、不変条件 1 つ。

**不変条件。** 任意の預金者について、ある瞬間に引き出せる価値は:

```
their_value = their_shares × total_assets / total_shares
```

deposit は**すべての既存預金者**についてこの不変条件を保たねばならない: deposit 前の価値 = deposit 後の価値。withdrawal も同じ: 残りの預金者の価値は不変。NAV 更新は全員の価値を同じ比率で変える。

**Deposit。** `programs/openhl-core/src/lib.rs:2327–2335` の `process_vault_deposit` から:

```rust
let shares_to_mint: u64 = if vault.total_shares == 0 || vault.total_assets == 0 {
    assets
} else {
    let numer = (assets as u128) * (vault.total_shares as u128);
    let s = numer / (vault.total_assets as u128);
    if s > u64::MAX as u128 {
        return Err(ProgramError::ArithmeticOverflow);
    }
    s as u64
};
```

分岐 2 つ。最初の deposit（`total_shares == 0`）は assets と shares を 1:1 で mint する — 補間する NAV 履歴がない。以降の deposit は:

```
shares_minted = assets_in × total_shares / total_assets
```

代数的に: これは `assets_in` を現行 share あたり NAV で**shares で表現**した値。deposit 後:

```
new_total_shares = total_shares + shares_minted
new_total_assets = total_assets + assets_in
new_NAV_per_share = new_total_assets / new_total_shares
                  = (total_assets + assets_in)
                    / (total_shares + assets_in × total_shares / total_assets)
                  = total_assets × (total_assets + assets_in)
                    / (total_assets × total_shares + assets_in × total_shares)
                  = total_assets / total_shares
                  = old_NAV_per_share
```

share あたり NAV は不変。不変条件は保たれる。

**Withdrawal。** `lib.rs:2452–2459` の `process_vault_withdraw` から:

```rust
let assets_to_return: u64 = {
    let numer = (shares_to_burn as u128) * (vault.total_assets as u128);
    let a = numer / (vault.total_shares as u128);
    if a > u64::MAX as u128 {
        return Err(ProgramError::ArithmeticOverflow);
    }
    a as u64
};
```

同じ数学を逆向きに:

```
assets_returned = shares_burned × total_assets / total_shares
```

withdrawal 後:

```
new_total_shares = total_shares - shares_burned
new_total_assets = total_assets - assets_returned
new_NAV_per_share = (total_assets - shares_burned × total_assets / total_shares)
                  / (total_shares - shares_burned)
                  = ... （同じ代数を逆向きに） ...
                  = total_assets / total_shares
                  = old_NAV_per_share
```

withdrawal も不変条件を保つ。

**NAV 更新。** `lib.rs:2535–2536` の `process_vault_update_nav` から:

```rust
let prev = vault.total_assets;
vault.total_assets = new_total_assets;
```

マネージャが新しい `total_assets` を書く。`total_shares` は不変。だから share あたり NAV は `prev / total_shares` から `new_total_assets / total_shares` に動く。すべての預金者の価値が同じ因子で動く。これが PnL の共有のされ方だ。

この 3 つ — deposit、withdraw、NAV 更新 — が vault 会計モデル全体。それ以外（gates、fees、vesting、制限）はその上のポリシーだ。

**整数除算と dust。** `/` 演算は整数除算。`total_assets = 1000`、`total_shares = 1000` の vault に 7 assets を預けるユーザは `7 × 1000 / 1000 = 7` shares を mint する — 綺麗。`total_assets = 1000`、`total_shares = 999` の vault に 7 assets を預けるユーザは `7 × 999 / 1000 = 6` shares を mint する（`6993 / 1000`、整数切り捨て）。その余分な 1/1000 share が dust — ユーザは 7 assets を払ったが deposit 時点の NAV 価値で 6.993 shares 分相当を受け取った。dust は実質的に 0.007 shares 分を残りの預金者に寄付する（彼らの share あたり価値がわずかに上がる）。

これは vault では一般的に許容される、(1) dust が丸め誤差スケール、(2) 新規預金者より既存預金者に有利で、保守的な方向だから。正確な保存が必要なプログラム（他で担保として使う yield-bearing トークンなど）には、スケールされた u128 share 表現や固定小数点算術による追加精度が要る。本書では意図的にそれを追加しない — コードが増えるだけで教育的価値が加わらない。

> **演習 §12.2.** 空の vault から始める。100 assets を預ける（預金者 A）。NAV を 200 に設定する（価格が倍）。預金者 B が 100 assets を預ける。B は何 shares 受け取るか。B は今 vault のどの割合を所有するか。

---

## §12.3  `VaultDeposit` を歩く

2282–2413 行の `process_vault_deposit` は 4 つのハンドラの中で最も複雑だ、最初の deposit で VaultShare PDA を条件付きで作成するからだ。構造を分解:

**検証**（2293–2318 行）: ペイロード サイズ、非ゼロ deposit、預金者は signer、vault は正しい所有者 + サイズ、system プログラムは正しい、share PDA は派生と一致する。

**vault 状態を読み mint する shares を計算**（2320–2342 行）: vault データを借用、最初の deposit（1:1）vs 以降（pro-rata）で分岐。

**vault 集約を更新**（2344–2353 行）:

```rust
vault.total_shares = vault.total_shares.checked_add(shares_to_mint)?;
vault.total_assets = vault.total_assets.checked_add(assets)?;
drop(vault_data);
```

`checked_add`（`saturating_add` ではない）: 加算がオーバーフローしうるなら、静かにキャップする代わりに deposit を拒否する。`u64::MAX` shares を超えて deposit を受け入れる vault は別の問題を抱えている。明示的な `drop(vault_data)` が share アカウントに触れる前に可変借用を解放する — share 作成が vault アカウント チェックパスを CPI で逆戻りしうるので必要だ。

**share アカウントの条件付き create-or-update**（2355–2403 行）:

```rust
let share_exists = share_ai.owner == program_id && share_ai.data_len() == VaultShare::LEN;
if !share_exists {
    let rent = Rent::get()?.minimum_balance(VaultShare::LEN);
    let create_ix = system_instruction::create_account(...);
    invoke_signed(...)?;
    // ... VaultShare フィールドを書く ...
} else {
    // ... 既存 shares + cost_basis に足す ...
}
```

「存在?」チェックは所有者 + data_len で — アカウントが本書の所有で正しいサイズなら、以前作成した VaultShare と仮定する。（discriminator チェックは `else` 分岐内でデータをキャストするときに起きる。）本書のものでなければ、標準 `invoke_signed` + `create_account` パターンで作る。

ユーザの最初の deposit が VaultShare アカウントの rent を払う（小さな一度限りのコスト）。以降の deposit はフィールドをインクリメントするだけ。これが慣例的なパターン — 代替は別の `CreateVaultShare` 命令を最初に呼ぶよう要求することだが、利益なしに摩擦が加わる。

ハンドラはどちらの分岐が走ったとしても最終的に share アカウントを所有しなければならない。両分岐とも最終状態は `share.shares` が預金者の総保有を反映し、`share.cost_basis` が累積預金を反映する。deposit は外から見えなくなる — 結果として残る share 状態だけが重要だ。

> **演習 §12.3.** ユーザが 100、50、25 を 3 つの別トランザクションで預金する。間に UpdateNAV なしで vault の NAV は一定。各ステップでユーザの share アカウントをダンプせよ。shares 数は線形に成長し、cost_basis は累計和になるはず。

---

## §12.4  `VaultWithdraw` を歩く

deposit より単純、作成するものがないから。2415–2500 行の `process_vault_withdraw`:

**返す assets を計算**、2452–2459 行 — §12.2 で扱った deposit 式の逆。

**認可**、2470–2473 行:

```rust
if share.owner != *owner_ai.key.as_ref() {
    msg!("vault_withdraw: caller is not the share owner");
    return Err(ProgramError::IllegalOwner);
}
```

share の記録された所有者だけが burn できる。これは share **単位**の認可で、vault 全体ではない — UpdateNAV のマネージャ チェックと異なる。この設計には「vault 管理者が任意の share を強制清算できる」経路はない（本物の本番 vault はコンプライアンス上の理由で追加するかもしれない）。

**残高十分性チェック**、2474–2480 行:

```rust
if share.shares < shares_to_burn {
    return Err(ProgramError::InsufficientFunds);
}
```

保有以上の shares は burn できない。

**Cost basis 削減**、2484–2489 行:

```rust
let basis_reduction = (((shares_to_burn as u128) * (share.cost_basis as u128))
    / (share.shares as u128 + shares_to_burn as u128)) as u64;
share.cost_basis = share.cost_basis.saturating_sub(basis_reduction);
```

比例削減。100 shares、cost_basis 1000 のユーザが 25 shares を burn すると、cost_basis は `25 × 1000 / 100 = 250` 減り、残り 75 shares の cost_basis は 750 になる。これが share あたり cost_basis を部分 withdrawal を通じて一定に保ち、P&L 報告が欲しい形になる。

分母の `as u128 + shares_to_burn as u128` は `share.shares` を**減算前**で使う（まだ減算していないから）。`share.shares -= shares_to_burn` の後の単純な `share.shares as u128` は誤った basis を計算する。

**集約を更新**、2491–2493 行 — `total_shares -= shares_to_burn`、`total_assets -= assets_to_return`。両方とも防御的に `saturating_sub`、事前チェックを考えれば underflow するはずがないが。

> **演習 §12.4.** §12.2 の演習の vault で、預金者 A に全 shares を引き出させる。A は何 assets 受け取るか。結果の vault 状態は（total_shares、total_assets）? 預金者 B の主張可能価値は変わっていないことを確認せよ。

---

## §12.5  マネージャ信用問題 — `VaultUpdateNAV`

2502–2544 行の `process_vault_update_nav` は短いが、ここに vault モデル全体の信用前提が住む:

```rust
if vault.manager != *manager_ai.key.as_ref() {
    msg!("vault_update_nav: caller is not the vault manager");
    return Err(ProgramError::IllegalOwner);
}

let prev = vault.total_assets;
vault.total_assets = new_total_assets;
```

マネージャは `total_assets` を任意の数値に更新するトランザクションに署名する。この数値がマネージャの実際の取引 PnL を反映していることをオンチェーンで検証する仕組みはない。**預金者はマネージャを信用する**。

本番でこれを硬化する 3 つのパターン:

**(1) オンチェーンで参照状態から NAV を計算する。** `new_total_assets` をペイロードで受け入れる代わりに、ハンドラが vault のオープン Position アカウントを読み、（第 11 章と同じ `compute_equity` を使って）その equity を合計し、結果を書く。これでマネージャは嘘をつけない — `total_assets` は機械的に導出される。コスト: UpdateNAV 呼び出しごとに参照されるアカウントが大幅に増え（ポジションあたり 1 つ）、CU とアカウントリスト上限を押す。

**(2) withdrawal を stated NAV ではなくオラクル価格で許す。** withdrawal は透明なオンチェーン規則（例: 公式による NAV、マネージャ報告による NAV ではなく）に基づいて受け取る assets を計算する。マネージャの NAV 報告は redemption の基礎ではなく advisory メタデータになる。

**(3) 遅延付き 2 段階 NAV 更新。** マネージャが新 NAV を提案、変更はある遅延後に適用（例: 1 時間）、遅延中、マネージャが嘘を報告していると思う預金者は**古い** NAV で引き出せる。これは一部の Curve/Yearn vault が使う trust-but-verify パターン。

本章はパターン (0) を出荷する — 検証なし、マネージャは信用される。教育用と小規模デプロイ vault には十分だが、本番化時のセキュリティ監査を始める正しい場所だ。

パターン (1) が理論的にこれほど魅力的なのに実務で稀な理由: N 個のポジションにわたって equity を合計するには N アカウントをロードする必要があり、N は本物の vault で数百になりうる。1 トランザクションでの約 64 アカウント上限と CU バジェットが、1 トランザクションでまとめられるポジション数に厳格な上限を置く。本番 vault は同時ポジション数を小さく制限するか、NAV 更新を複数トランザクションに分けるかのどちらかだ。

> **演習 §12.5.** マネージャが `total_assets` を `u64::MAX`（悪意ある更新）に設定したら何が起きるかを辿れ。既存預金者への即時効果は? 新規預金者への効果は? 誰かが引き出そうとしたときの最終結果は?

---

## §12.6  シングルトン書き込みの再起 — 第 5 章アンチパターン再び

`TradingVault` は本コードベースが第 5 章の `Stats` 警告以来持っている典型的なシングルトン書き込み共有アカウントだ。すべての deposit、withdrawal、NAV 更新が同じ `(total_shares, total_assets)` ペアを書く。異なるユーザからの 2 つの同時 deposit は並列に走れない — 両方とも vault 集約を書き、Sealevel が直列化する。

どれほど悪いか? 1 秒の deposit レイテンシで、vault は slot あたり 1 deposit、最大約 2.5 deposit/秒を受け入れる。戦略間で資本を動かす数千の預金者を持つ vault には、これがユーザ体験の binding 制約。

3 つの緩和策、すべて本物、すべて本番で異なる vault が使う:

**(1) オフチェーン deposit キュー。** deposit はオフチェーン キュー（Redis、データベース）に書かれる。定期的なオンチェーン「batch settle」命令が N deposit を 1 トランザクションで処理し、多くのユーザに対してシングルトン書き込みコストを 1 回払う。トレードオフ: deposit はもうアトミックでなく — ユーザは「pending」ステータスを見て、数分後に「confirmed」を見る。ほとんどの機関 vault はこう動く。「待つけど ET の午後 4 時に戦略に入る」パターン。

**(2) vault をシャーディングする。** N 個の独立 `TradingVault` アカウントを持ち、それぞれが自分の (total_shares, total_assets) を持つ。deposit は預金者の pubkey ハッシュに基づいてシャードにルートする。読み取りは全シャードで集約する。これがシングルトンを破る — N シャードは N 並列 deposit を意味する。トレードオフ: NAV 更新が今や N トランザクションを要し、シャード間のリバランスが事になる。実世界例: 大きな Curve/Yearn vault はまさにこの理由でシャーディングすることがある。

**(3) deposit ごとの蓄積子。** deposit ごとにシングルトンを更新する代わりに、個別 deposit「チケット」がユーザごとのアカウントに書かれ、定期的な「checkpoint」呼び出しがそれらをシングルトンにロールする。オプション 1 と似て見えるがオンチェーンに留まる — キューは未決済チケット アカウントの集合だ。トレードオフ: 決済の複雑さ、deposit と share 発行の間のわずかな遅延。

本章はオプション (0) を出荷する — 標準的な同期 deposit。低スループット vault（< 10 deposit/秒）には十分で、教育的に最も明快だ。(1) や (3) を通る本番経路は踏み固められており、本章のスコープ外だ。フレーミングは「なぜ欲しいかが今わかる」。

より深いレッスン、第 5 章の言い直し: **すべてのシングルトン書き込みは将来のスケーリング ボトルネック**。「totals」や「aggregate」アカウントに手を伸ばしている自分を見つけたら、同じ意味論を持たずに表現できるか問え。時に答えは yes（第 10 章のポジションごとの決済 — 集約不要）、時に答えは no で上の緩和パターンが要る。要点は、負荷下で発見するのではなく意識的にトレードを行うこと。

---

## §12.7  まとめと自己検証

### まとめ図

```
Vault ライフサイクル:

  manager ──► CreateTradingVault ──► TradingVault{shares=0, assets=0}
                                            │
   user A が 100 deposit ──► VaultDeposit ──► │
                                            ▼
                          TradingVault{shares=100, assets=100} (最初は 1:1)
                          VaultShare_A{shares=100, basis=100}
                                            │
   manager が取引 ──► UpdateNAV(200) ────►   │
                                            ▼
                          TradingVault{shares=100, assets=200} (NAV ×2)
                                            │
   user B が 100 deposit ──► VaultDeposit ──► │  shares = 100 × 100 / 200 = 50
                                            ▼
                          TradingVault{shares=150, assets=300}
                          VaultShare_B{shares=50, basis=100}

   A が全 withdraw ──► VaultWithdraw(100) ──► assets_out = 100 × 300 / 150 = 200
                                            ▼
                          TradingVault{shares=50, assets=100}
                          VaultShare_A{shares=0, basis=0}
                          VaultShare_B{shares=50, basis=100} (不変)


シングルトン書き込み再起（第 5 章再び）:

  4 つの命令すべてが TradingVault アカウントを WRITE する:
    deposit / withdraw / update_nav / create

  ⇒ Sealevel がすべての vault 操作を他のすべてに対して直列化する。
  ⇒ スループット上限: vault あたり slot あたり約 1 op。

  緩和策（ここでは実装しない）:
    - オフチェーン deposit キュー → batch settle
    - vault を N 個の独立集約にシャーディング
    - deposit ごとの蓄積子 + 定期的 checkpoint
```

### 自分で検証する 3 項目

1. **deposit を通じての NAV 保存。** 空の vault から始めよ。A に 100 deposit させる。即座に B に 100 deposit させる（間に UpdateNAV なし）。vault は `total_shares = 200, total_assets = 200` のはず、A と B の share あたり NAV は両方とも 1.0 でなければならない。次に C に 100 deposit させる。同じ share あたり NAV: 1.0。
2. **NAV 更新が全員の価値を一律に変える。** 上の状態から `vault --update-nav --total-assets 600`（3× 価値）を走らせる。A の `claimable_assets = A.shares × total_assets / total_shares = 100 × 600 / 300 = 200`。B と C も同じ。3 預金者全員が 3× ゲインを比例的に共有する。
3. **非 1:1 NAV での withdrawal。** 上から（各預金者が各々 200 assets 価値の 100 shares を持つ）、A に全 100 shares を withdraw させる。200 assets を受け取る。vault は今 `total_shares = 200, total_assets = 400`。B と C はそれぞれまだ 100 shares 所有（vault の 50%）、各々 200 assets 価値 — A の退出で不変。

---

## 第 13 章への導線

預金者資本をプールし pro-rata で PnL を分配する vault を持つようになった。**まだ持っていない**のは**vault が実際に取引する仕組み**だ。§12.5 のマネージャ NAV 更新は主張であって検証された行動ではない — 「vault マネージャが vault の資産を使ってポジションをオープンする」命令はない。それを追加するのが Phase B 統合弧の自然な次ステップだ: vault の PDA が所有する Position を作る（vault のシードで `invoke_signed`）マネージャ署名の `VaultOpenPosition`、vault の追跡された assets から担保を引く。

第 13 章は builder codes を組み立てる — それを通じてユーザがルートされたトレーディング フロントエンドが手数料の一部を集められる、プロトコル ネイティブの紹介 / 手数料分配仕組みだ。Builder codes はプログラム内のすべての手数料を持つ命令（place_order、本番エスクロー パスの deposits、liquidations）に触れ、各トランザクションの AccountMeta に fee_recipient アカウントを加える。本章では手数料分割がどう基礎アクションとアトミックに起きるか（別の「claim fees」呼び出しが不要）と、Solana DEX フロントエンドを独立したビジネスとして成立させる分配インセンティブを builder-code 構造がどうエンコードするかを探る。
