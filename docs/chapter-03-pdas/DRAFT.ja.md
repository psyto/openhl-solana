# 第3章 — PDA を原理から組み立てる

> 状態: ドラフト (v0.1)。
> 教材コード: [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（`process_create_market` ハンドラ）、[`scripts/create-market/src/main.rs`](../../scripts/create-market/src/main.rs)。
> 検証対象バージョン: solana-pubkey 2.4.0、solana-cpi 2.2.1、solana-system-interface 1.0.0、solana-program 2.3.0。

---

## §3.0  はじめに

第 1 章と第 2 章で扱ったアカウントは、すべて**アドホックな**鍵ペアで識別されていた — クライアント側で `Keypair::new()` を生成し、作成・割当のトランザクションに署名させ、その後は破棄する。単体デモには通用するが、それ以外には通用しない。

3 週間後にユーザが `(SOL, USDC)` の market アカウントを引きたいと考えた場合を想像しよう。ユーザ自身がその market を作ったわけではない。鍵ペアも持っていない。「全 market を走査する」オンチェーン台帳もない。アカウントを見つける唯一の方法は、プログラム側がその住所を**指定**することだ — `(base_mint, quote_mint)` とプログラム ID という識別パラメータから、誰でも再計算できる形でアドレスを導出する。対応する秘密鍵を持たない、プログラムに紐付いた公開鍵派生である。

それが Program-Derived Address (PDA) だ。本章では次の順に進める。

1. `solana-pubkey` を開き、PDA 派生アルゴリズムを読む — シードとプログラム ID と特別なマーカーの sha256、そして結果を ed25519 曲線から外すための 1 バイト「bump」。
2. `find_program_address`（およびその兄弟 `create_program_address`）を 1 行ずつ歩き、片方が反復し片方が反復しない理由を理解する。
3. `solana-cpi` を開き、`invoke_signed` を読む。プログラムが自分の所有する PDA に対して、シードをランタイムへ渡し直すことで「署名」する仕組みを見る。
4. 新しい `process_create_market` ハンドラを歩く: PDA を派生し、呼び出し元が正しいアカウントを渡したか検証し、System へ CPI でアロケート要求を出し、レイアウトを書き、bump を保存する。
5. 単一トランザクションが第 2 章の 2 段階 `[Assign, Initialize]` フローを置き換える瞬間を観察する。
6. 同じ `(base_mint, quote_mint)` でクライアントを再実行し、System プログラムが重複作成を拒否するのを見る — アドレスは固定されている。

終えるころには、任意の Anchor `#[account(init, seeds = [...], bump)]` 制約を見て、それが展開する派生・CPI・署名の全段取りを辿れるようになる。そして「プログラムが、渡されたアカウントが派生 PDA と一致するか検証し忘れた」から始まる PDA 系のエクスプロイトがなぜ多いのかも理解できる。

---

## §3.1  PDA アルゴリズム

通常の Solana 公開鍵は、ed25519 楕円曲線上の 32 バイトの点である。対応する秘密鍵があり、その秘密鍵がトランザクションに署名する。PDA は意図的に曲線上の**点ではない** — 32 バイトのハッシュであって、結果として曲線の**外側**に落ちる。つまり、どの ed25519 秘密鍵もその PDA に対する署名を生成できない。クライアント側からは絶対に署名できない。それを派生したプログラムだけが、`invoke_signed` を通じて使用を許可できる。

アルゴリズムは `solana-pubkey-2.4.0/src/lib.rs:911–958` にある。ハッシュ構築そのものは 928–933 行。

```rust
let mut hasher = solana_sha256_hasher::Hasher::default();
for seed in seeds.iter() {
    hasher.hash(seed);
}
hasher.hashv(&[program_id.as_ref(), PDA_MARKER]);
let hash = hasher.result();
```

ハッシュへの入力は 3 種類。

1. **各シード**を、連結する順に投入。
2. **プログラム ID**（32 バイト）。
3. **`PDA_MARKER`**、`lib.rs:52` に定義された 21 バイト定数。
   ```rust
   const PDA_MARKER: &[u8; 21] = b"ProgramDerivedAddress";
   ```

このマーカーがあることで、誰かが**通常の**鍵ペアを生成して PDA と衝突する公開鍵を作る、という攻撃を阻止できる。本物の ed25519 鍵はシード材料の後ろに `b"ProgramDerivedAddress"` を付けてハッシュして生成されることがない以上、PDA は通常鍵と取り違えられない。

ハッシュの後、32 バイトのダイジェストは曲線判定にかけられる。`lib.rs:935–937`。

```rust
if bytes_are_curve_point(hash) {
    return Err(PubkeyError::InvalidSeeds);
}
```

ハッシュがたまたま ed25519 曲線上に落ちた場合は拒否される。理由は、そのアドレスは原理的にはどこかの秘密鍵で署名されうるからだ（確率は 50%）。PDA の存在意義は、派生プログラム以外の誰も署名できないことにある。候補シードのおよそ半数が曲線上に落ち、拒否される。

ランタイムは構造的な上限も強制する。`lib.rs:45–47`。

```rust
pub const MAX_SEED_LEN: usize = 32;
// ...
pub const MAX_SEEDS: usize = 16;
```

シードは最大 16 個、それぞれ最大 32 バイト。通常の使い方ではまず到達しない。最悪のシステムコールコストを上限で抑えるためのものだ。

**SDK が隠していること:** Anchor の `seeds = [b"market", base_mint.key().as_ref(), quote_mint.key().as_ref()]` 制約は、コード生成時にこの同じアルゴリズムへシード配列を直接渡している。制約はあわせて構造体に `bump` フィールドも書き込む — §3.2 で見る同じバイトだ。Anchor はその由来を見せないだけである。

> **演習 §3.1.** `create_program_address` に空のシードリストを渡すと何が起きるか。関数冒頭を確認せよ。挙動は意図的だが、見落としやすい。

---

## §3.2  `find_program_address` vs `create_program_address` — bump 反復

任意のシード入力の半数は曲線上に落ち、失敗する。それなら、与えられたシードセットに対して有効な PDA をどう見つけるか。1 バイトのカウンタ（**bump**）を末尾に追加し、255 から始めて `create_program_address(seeds || [bump])` を試し、曲線外のハッシュが出るまで bump を 1 ずつ減らす。これが `find_program_address` だ。

`solana-pubkey-2.4.0/src/lib.rs:823–862` から（オフチェーン側のパス）。

```rust
pub fn try_find_program_address(seeds: &[&[u8]], program_id: &Pubkey) -> Option<(Pubkey, u8)> {
    #[cfg(not(target_os = "solana"))]
    {
        let mut bump_seed = [u8::MAX];
        for _ in 0..u8::MAX {
            {
                let mut seeds_with_bump = seeds.to_vec();
                seeds_with_bump.push(&bump_seed);
                match Self::create_program_address(&seeds_with_bump, program_id) {
                    Ok(address) => return Some((address, bump_seed[0])),
                    Err(PubkeyError::InvalidSeeds) => (),
                    _ => break,
                }
            }
            bump_seed[0] -= 1;
        }
        None
    }
    // (target_os = "solana" ブランチは sol_try_find_program_address syscall に委譲)
}
```

関数は 255、254、253... と試し、最初に成功した値を返す。「正規 bump」は**最も大きい**有効値 — 最初に試される値だ — である。クライアントとプログラムが同じ PDA を派生する必要があり、「最大の有効 bump」は一意の決定論的答えになるからだ。

最初の有効 bump が 255 であるシード（よくある）なら、ループは 1 回で済む。運の悪いシード組み合わせだと数回回り、オンチェーンで 1 回あたり約 1,500 CU を消費する。このコストを毎 CPI で払わないために、プログラムは `find_program_address` が成功した最初の機会に **bump をアカウントデータに保存**する。以降は `create_program_address(seeds || stored_bump)` を使えば反復はゼロだ。`process_create_market` がまさにそれを `lib.rs:312` で行っている。

```rust
market.bump = bump;
```

対照的に `create_program_address` は、完全に指定されたシードリストを受け取って、成功か失敗を返すだけだ。**検証**には適している（安価で決定論的）が、**探索**には不向きだ（試すべき bump を知らない）。探索と保存は `find_program_address` で 1 度行い、以降の検証は `create_program_address` で済ませる。

**SDK が隠していること:** Anchor の `bump` 制約は `init` 時に bump を保存し、以降のアクセス時の `bump = market.bump` 形式は保存済みの値を使う（反復しない）。最適化は本書と同じ。隠れているのは保存場所だけだ。

> **演習 §3.2.** シード接頭辞を 3 つ選んで（例: `b"market"`、`b"position"`、`b"vault"`）、それぞれに `find_program_address(&[prefix, &[0u8; 32], &[0u8; 32]], &your_keypair.pubkey())` を小さな Rust テストで呼んでみよ。返ってきた bump を記録すること。少なくとも 2 つは 255 になるはずだ。なぜ 255 がそれほど頻繁に出るのか。

---

## §3.3  `invoke_signed` — プログラムが自身の PDA のために署名する仕組み

PDA には秘密鍵がない。では、新アカウントが**署名**する必要のある `System::create_account` のような CPI を、新アカウントが PDA であるときどうやって成立させるのか。

答え: **プログラム**が PDA のために署名する。シード（bump 含む）を CPI と一緒に渡すことで。ランタイムはそのシードとプログラム ID から PDA を再派生し、CPI が操作対象としているアカウントと一致すれば、プログラムを署名者として受け入れる。

`solana-cpi-2.2.1/src/lib.rs:251–273` から。

```rust
pub fn invoke_signed(
    instruction: &Instruction,
    account_infos: &[AccountInfo],
    signers_seeds: &[&[&[u8]]],
) -> ProgramResult {
    // ...
    invoke_signed_unchecked(instruction, account_infos, signers_seeds)
}
```

`signers_seeds` パラメータの型は `&[&[&[u8]]]` — シードセットのスライスで、署名対象 PDA ごとに 1 セットだ。各内側 `&[&[u8]]` は、`create_program_address` に渡すものとまったく同じ形。

本書での呼び出しは `programs/openhl-core/src/lib.rs:297–301`。

```rust
invoke_signed(
    &create_ix,
    &[payer_ai.clone(), market_ai.clone(), system_ai.clone()],
    &[&[MARKET_SEED, base_mint.as_ref(), quote_mint.as_ref(), &[bump]]],
)?;
```

`invoke_signed` に渡すシードは、**PDA を派生したときと同じシード**に bump を加えたものだ。ランタイムはそれをハッシュし、結果が `market_ai.key` と等しいことを確認し、そのアカウントの署名者としてプログラムを扱う。CPI はあたかも market PDA が本物の ed25519 署名をしたかのように進む。

決定的な不変条件: ハッシュに埋め込まれるプログラム ID は**常に呼び出し元プログラム**だ。**別の**プログラムの ID から派生した PDA に対して `invoke_signed` することはできない。これが、PDA の所有権をプログラムローカルに閉じ込めている仕組みだ — 派生したプログラムだけが、そのアドレスに署名できる。

**SDK が隠していること:** Anchor の `init` 制約は、まさにこの `invoke_signed` 呼び出しを生成する。シードは `seeds = [...]` 制約から、bump は `bump` ストレージから取る。マクロ生成のため CPI は目に見えないが、コードは同じだ。

> **演習 §3.3.** `process_create_market` を編集し、**間違った** bump（例: `bump.wrapping_sub(1)`）で `invoke_signed` を呼ぶよう書き換えよ。どのエラーが返るか。ランタイムのどのチェックで弾かれているか辿ること。

---

## §3.4  `process_create_market` — プログラム側

これでプログラムを歩ける。`programs/openhl-core/src/lib.rs:196–321` から。ハンドラ内の番号付き 6 ステップ。(1)〜(3) はペイロードサイズ、署名者チェック、System プログラム ID 一致チェック — 単純なパラメータ検証だ。面白い部分は (4) から始まる。

```rust
// (4) Derive the expected PDA from the payload fields + program_id, and
// verify the caller passed us the right account. This is what binds
// a `(base_mint, quote_mint)` pair to a single, predictable address.
let (expected_pda, bump) = Pubkey::find_program_address(
    &[MARKET_SEED, base_mint.as_ref(), quote_mint.as_ref()],
    program_id,
);
if market_ai.key != &expected_pda {
    msg!(
        "create_market: passed market {} != derived PDA {}",
        market_ai.key,
        expected_pda
    );
    return Err(ProgramError::InvalidSeeds);
}
```

`market_ai.key != &expected_pda` のチェックこそが構造全体を安全にする。これがなければ、プログラムは呼び出し元がスロット 1 に入れた任意のアカウントに対して操作を始めてしまう — `invoke_signed` のシード対鍵チェックが後で不整合を捕捉するが、よりわかりにくいエラーで落ちる。明示的な検証なら、綺麗なメッセージで即座に失敗する。

ここに微妙な点が 1 つある。`process_create_market` では常に `find_program_address`（反復する側）を呼んでいる。安価な代替があるのになぜか。なぜ `create_program_address(seeds || [bump])` を、クライアント供給の bump を信じて使わないのか。アカウントがまだ存在しないから — 読み込める `market.bump` がオンチェーンにない。クライアントが命令データで bump を渡す手はあるが、それでもプログラムは検証し直さなければならない。`find_program_address` をここで使うのは、作成時 1 回だけ約 1500 CU を払うトレードだ。この market への以降の操作（後の章で扱う）はアカウントから `market.bump` を読み、無料の `create_program_address` で検証する。

ステップ (5) は §3.3 で見た CPI そのもの — bump を最後のシードに加えた `invoke_signed`。ステップ (6) は Market レイアウトの書き込みで、第 2 章の `process_initialize` とほぼ同じ。違いは `lib.rs:312` の 1 行だけだ。

```rust
market.bump = bump;
```

bump がアカウント自身に永続化される。今後この market に触れる任意の命令 — `place_order`、`cancel`、`settle` — は `market.bump` を読み、`create_program_address` で無料に検証する。

> **演習 §3.4.** 明示的な PDA 検証（`if market_ai.key != &expected_pda` ブロック）を削除せよ。ビルド・デプロイし、PDA の代わりに新しい `Keypair::new()` を market アカウントとして渡す `CreateMarket` トランザクションを組み立てよ。ランタイムはどのエラーを返すか。明示チェックを残す価値はなぜあるか。

---

## §3.5  クライアント — 1 命令、市場鍵ペア不要

クライアント側は大幅に簡略化される。`scripts/create-market/src/main.rs` から。

```rust
let (market_pda, bump) = Pubkey::find_program_address(
    &[MARKET_SEED, base_mint.as_ref(), quote_mint.as_ref()],
    &program_id,
);
// ...
let ix = Instruction {
    program_id,
    accounts: vec![
        AccountMeta::new(payer.pubkey(), true),
        AccountMeta::new(market_pda, false),
        AccountMeta::new_readonly(system_program::ID, false),
    ],
    data,
};

let tx = Transaction::new_signed_with_payer(
    &[ix],
    Some(&payer.pubkey()),
    &[&payer],
    blockhash,
);
```

第 2 章の `init-market` と比べてみよう。あちらでは次のものが必要だった。

- クライアント側で生成した market `Keypair::new()`
- 2 つの命令: `[System::Assign, openhl-core::Initialize]`
- 2 名の署名者: `[&payer, &market]`

本章のバージョンには次しかない。

- シードから派生される market **PDA**（鍵ペアなし、秘密なし）
- 1 つの命令: `openhl-core::CreateMarket`
- 1 名の署名者: `[&payer]`

market アカウントは `AccountMeta::new(market_pda, false)` で渡される — 書き込み可能、署名者ではない。署名する鍵ペアがない。署名はプログラム内部で発生し、`invoke_signed` がシードを供給する。

クライアントが使う `MARKET_SEED` 定数はプログラムクレートから直接インポートしている。`scripts/create-market/Cargo.toml`。

```toml
openhl-core = { path = "../../programs/openhl-core", features = ["no-entrypoint"] }
```

`no-entrypoint` 機能を有効にすることで、プログラムの型と定数はホストバイナリから利用できるようになり、BPF エントリポイントは持ち込まれない。クライアントは `openhl_core::MARKET_SEED` を使う — クライアントとプログラムがシード接頭辞についてコピペなしで合意する保証になる。

> **演習 §3.5.** `create-market` を同じ `--base-mint` と `--quote-mint` で 2 回実行せよ。2 回目は失敗する。バリデータが返すエラーは System プログラムに言及している。System プログラムのソース（`solana-system-interface-1.0.0/src/error.rs`）でそれを見つけ、バリアントを特定せよ。

---

## §3.6  `#[account(init, seeds = [...], bump)]` が実際に生成しているもの

Anchor の PDA 版 `init` 制約は、本章で行った作業のすべてに、第 1 章と第 2 章で行った作業を上乗せした形に展開される。突き合わせて並べると次のとおり。

| Anchor がやること | 平文に展開すると |
|---|---|
| 属性から `seeds = [...]` を読む | `process_create_market` がペイロードから復号する（`lib.rs:235–251`） |
| `find_program_address(seeds, program_id)` で bump を取得する | `lib.rs:268–271` |
| bump を型付き `Account<T>` に保存し、以降のアクセスを安価にする | `lib.rs:312` の `market.bump = bump;` |
| `passed_account.key == derived_pda` をアサートする | `lib.rs:272–279`（明示チェック） |
| `Rent::get()?.minimum_balance(space)` を呼ぶ | `lib.rs:289` |
| `system_instruction::create_account(payer, pda, lamports, space, program_id)` を組み立てる | `lib.rs:290–296` |
| `invoke_signed(create_ix, accounts, &[seeds || bump])` を呼ぶ | `lib.rs:297–301` |
| Anchor の 8 バイトディスクリミネータを `data[0..8]` に書く | `lib.rs:310` の `market.discriminator = MARKET_DISCRIMINATOR;` |
| 型付き `Account<T>` を `Drop` 時にアカウントへシリアライズし戻す | `lib.rs:310–319` の in-place フィールド書き込み |

PDA ベースの `init` で責務は 9 つ。Anchor は 1 つの属性で表現し、本書は約 60 行を要した。

Anchor が行い本書が**まだ**やっていないことが 1 つある。以降のアクセスごとに**正規 bump チェック**を生成することだ。後で `#[account(seeds = [...], bump = market.bump)]` と書くと、Anchor は `create_program_address(seeds || bump)` を呼び、結果が渡されたアカウントの鍵と等しいことをアサートする。§3.2 で触れた安価な検証だ — アクセス 1 回ごとに支払い、反復はなし。今後の章では同じチェックが必要になる。本書では market に触れるたびに手で書く。

---

## §3.7  まとめと自己検証

### まとめ図

```
クライアント                           プログラム                              ランタイム
────────                             ─────────                              ──────────
find_program_address(           ──┐
  [b"market", base, quote],       │
  program_id) → (pda, bump)       │
                                  ▼
ix を組み立てる:                       process_create_market
  accounts: [payer S+W,                  find_program_address(
            pda W,                          [b"market", base, quote],
            system r/o]                     program_id) → (pda, bump)
  data: [tag=1, ...]                     assert market_ai.key == pda
                                         Rent::get().minimum_balance(...)
[payer] で署名                          invoke_signed(
                                           create_account(payer, pda,
send_and_confirm_tx           ─────►       lamports, space, program_id),
                                           accounts,
                                           &[&[b"market", base, quote, [bump]]]
                                         )
                                                                  │
                                                                  ▼
                                                            ランタイム: シードをハッシュ、
                                                            == pda を検証、
                                                            プログラムを署名者として受理、
                                                            System::create_account を実行
                                         Market レイアウトを書く（market.bump = bump 含む）
                                         Ok を返す
```

### 自分で検証する 3 項目

1. **決定論的アドレス。** `create-market` を実行し、表示された `market PDA` を控える。オンチェーンアカウントを削除する（または新しい `solana-test-validator` を使う）。同じ `--base-mint`、`--quote-mint`、`--program` で再実行する。表示されるアドレスはバイトレベルで同一のはずだ。次に `--base-mint` と `--quote-mint` の順序を入れ替えて実行する。アドレスは変わるはずだ — シードの順序は派生の一部だ。
2. **bump が保存されている。** `create-market` を実行した後、16 進ダンプはオフセット 9（`bump` フィールド）に非ゼロのバイトを示すはずだ。クライアントは自分が派生した bump と、プログラムが書き込んだ bump の両方を表示する。両者は一致しなければならない。[`scripts/create-market/src/main.rs:163`](../../scripts/create-market/src/main.rs#L163) で確認せよ。
3. **秘密鍵不要。** `solana account <market_pda>` で `Owner` がプログラム ID であることを確認せよ。`~/.config/solana/` 内に対応するエントリはなく、どこにも鍵ペアファイルはない — このアドレスには秘密鍵がない。これが PDA を永続的にする仕組みだ。盗もうにも盗む鍵が存在しない。

---

## 第 4 章への導線

プログラムがアカウントを所有でき、アドホックな鍵ペアではなく自前のシードからアドレスを派生できるようになった。しかしまだ**数えなければならない**場面には出くわしていない。これまでの命令はすべて、Solana のトランザクション単位コンピュートバジェット — 既定 200,000 コンピュートユニット (CU)、要求すれば最大 140 万 — の中に余裕で収まっていた。何かループするものを追加した瞬間 — 板マッチング、バッチ決済、ちょっと込み入った Borsh 構造体の復号でも — CU が最初の制約として効いてくる。

第 4 章ではコンピュートバジェットを実エンジニアリング上の関心事として導入する。`Initialize` 命令に CU 計測を追加し（`sol_log_compute_units` システムコール経由）、各部分のコストをベンチマークし、プログラムが `Box` や `Vec` を「無料」のように扱える錯覚を成立させているヒープアロケータを歩き、CU 上限の中に収まる必要のある `place_order` 命令を追加する。章末では、Phase A と Phase B の残りすべてを駆動する問いを立てる: 200,000 CU の中で動く板マッチャをどう書くか。
