# 第1章 — バイトから組み立てるアカウントモデル

> 状態: ドラフト (v0.1)。
> 教材コード: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs)、[`scripts/allocate-market/src/main.rs`](../../scripts/allocate-market/src/main.rs)。
> 検証対象バージョン: solana-sdk 2.3.1、solana-rent 2.2.1、solana-account 2.2.1、solana-system-interface 1.0.0。

---

## §1.0  はじめに

Solana のチュートリアルはたいてい Anchor から始まる。`#[derive(Accounts)]` の構造体を書き、`#[account(init, payer = signer, space = 8 + ...)]` のような属性をいくつか散りばめれば、数行のマクロで動くプログラムができあがる。マクロは魅力的だが、Solana ランタイムの全モデルを属性 1 行に畳み込んでしまう。マクロが誤った既定値を選んだとき、どこを見ればよいかわからない。マクロが裏で何をしているかを一度も見ていないからだ。

本章はその解毒剤である。次の順で進める。

1. `solana-account` を開き、`Account` の 5 つのフィールドを読む。
2. レント免除残高を手計算し、ランタイムの値と突き合わせる。
3. System プログラムを直接呼び、ローカルバリデータ上に 256 バイトのアカウントを確保する。
4. 生のバイトを 16 進ダンプし、自分たちのレイアウト定義と照らし合わせて各バイトの意味を確認する。
5. 上記すべてを `#[account(init, ...)]` がどう肩代わりしていたかを、行ごとに対応づけて列挙する。

終えるころには、Solana の任意のアカウントの任意のバイトを指して、それが何を表し、どこから来て、どのプログラムだけがそれを変更できるかを答えられるようになっている。本トラックのその後すべてはこの土台の上に立つ。

教材として扱う対象は、本物の成果物の最小単位 — HL 型パープ DEX の空の `Market` アカウントである。「空」なのは、作成後の所有者が System プログラムであり、System には「任意のバイトを書き込む」命令が存在しないからだ。この欠落こそが第 2 章への導線になる。

---

## §1.1  アカウントの 5 つのフィールド

`solana-account-2.2.1/src/lib.rs:44–56` を開こう。

```rust
#[repr(C)]
// ...
pub struct Account {
    /// lamports in the account
    pub lamports: u64,
    /// data held in this account
    pub data: Vec<u8>,
    /// the program that owns this account. If executable, the program that loads this account.
    pub owner: Pubkey,
    /// this account's data contains a loaded program (and is now read-only)
    pub executable: bool,
    /// the epoch at which this account will next owe rent
    pub rent_epoch: Epoch,
}
```

定義はこれだけ。5 つのフィールド。`slot` も `version` も `nonce` も `storage_root` もない。Solana ランタイムがアカウント「について」記録するのはこれだけで、それ以外の情報はすべて `data` フィールド内に不透明なバイトとして格納される。

1 つずつ見ていこう。

**`lamports: u64`** — アカウントの残高を lamport 単位で保持する（1 SOL = 10⁹ lamport）。任意のプログラムが残高を「増やす」ことはできる（送金するだけ）。しかし、残高を「減らす」ことができるのは、そのアカウントの**所有プログラム**だけだ。これはローダが強制するランタイム不変条件で、自分のものではないアカウントから引き落とそうとすると、トランザクションは関数が返るより前に失敗する。

**`data: Vec<u8>`** — アカウントの記憶領域。ランタイムから見れば不透明なバイトの並びで、長さの上限は 10 MB。プログラムはこのバイト列を好きなように解釈する（Anchor なら Borsh、本コードなら `bytemuck`、生のシェーダなら独自形式、と何でもいい）。書き込めるのは所有プログラムだけ。他のプログラムは読めるだけだ。

**`owner: Pubkey`** — `lamports` の減算と `data` の書き換えを許可されたプログラムの公開鍵。ウォレットなら System プログラム、SPL Token アカウントなら SPL Token プログラム、第 2 章以降の `Market` アカウントなら自前のプログラムが入る。所有者は作成時（System の `CreateAccount` または `Assign`）に一度だけ設定され、それ以降は現在の所有者が明示的に `Assign` を発行したときだけ切り替わる。

**`executable: bool`** — `data` に BPF プログラムがロードされていれば `true`、そうでなければ `false`。一度 `true` になったアカウントは読み取り専用に固定され、永久に書き換えられない。Solana における不変性はこの仕組みで成立している。アカウントを executable に切り替え、以降の書き込みをランタイムが拒否する、それだけだ。

**`rent_epoch: Epoch`** — 歴史的遺物。Solana 初期にはランタイムがアカウントサイズに応じて定期的にレントを徴収しており、このフィールドは次回課金タイミングを記録していた。レント徴収は実質的に廃止され、厳格なレント免除（§1.2）に置き換わった。フィールドは残っているが、もはや実務的な意味はほぼない。`solana account <pubkey>` の出力でも、変化しない数値として目に入るだけだ。

**SDK が隠していること:** Anchor で `pub user_data: Account<'info, UserData>` と宣言すると、受け取れるのは `data` フィールドを「解析済み」の型付きビューだ。残り 4 つのフィールド — `lamports`、`owner`、`executable`、`rent_epoch` — も裏の `AccountInfo` 経由で参照できるが、型はそのうち 1 つにしか目を向けさせない。多くの開発者は Anchor を使い続けるあいだ、`owner` を明示的に触らずに終わる。所有者チェック漏れが Solana プログラムの代表的なセキュリティバグであるにもかかわらず。

> **演習 §1.1.** SPL トークンアカウントのアドレスを 1 つ選び（たとえば自分の USDC 口座）、次を実行する。
> ```
> solana account <pubkey> --output json
> ```
> JSON 出力から 5 つのフィールドそれぞれを特定せよ。`owner` は何か。`data` は base64 でどう見えるか。`executable` は期待どおりか。

---

## §1.2  レントとレント免除

`solana-rent-2.2.1/src/lib.rs:32–45` を開こう。

```rust
#[repr(C)]
// ...
pub struct Rent {
    /// Rental rate in lamports/byte-year.
    pub lamports_per_byte_year: u64,

    /// Amount of time (in years) a balance must include rent for the account to
    /// be rent exempt.
    pub exemption_threshold: f64,

    /// The percentage of collected rent that is burned.
    pub burn_percent: u8,
}
```

そして計算式そのもの、`lib.rs:93–97`。

```rust
pub fn minimum_balance(&self, data_len: usize) -> u64 {
    let bytes = data_len as u64;
    (((ACCOUNT_STORAGE_OVERHEAD + bytes) * self.lamports_per_byte_year) as f64
        * self.exemption_threshold) as u64
}
```

規則はこうだ。アカウントの残高が、現在のレート換算で 2 年分のレントを賄えるだけあれば、そのアカウントは**レント免除**になる。閾値未満で作成しようとすればトランザクションは失敗する。稼働中のアカウントの残高をこの閾値より下げようとしても（たとえば lamport を引き出すと）、同様に失敗する。

`lib.rs:70` の `ACCOUNT_STORAGE_OVERHEAD` 定数が肝心の落とし穴である。

```rust
pub const ACCOUNT_STORAGE_OVERHEAD: u64 = 128;
```

どんなに小さなアカウントでも、すべてのアカウントは「追加 128 バイト分」の課金を受ける。これはランタイム側の管理コスト — §1.1 のメタデータフィールド、インデックスのオーバーヘッドなど — を反映している。だから「ゼロバイトのアカウント」と「128 バイトのアカウント」のレントは同じだ。

`Market`（データ 256 バイト）の計算は次のとおり。

```
minimum_balance = (128 + 256) × lamports_per_byte_year × 2.0
                = 384 × 3480 × 2
                = 2,672,640 lamports
                ≈ 0.00267 SOL
```

（`lamports_per_byte_year` の既定値は約 3480。`lib.rs:54` で「1 MB あたり 1 日 $0.01」から導出されている。）

本書のスクリプトはこの値をローカル計算ではなく RPC に問い合わせる。

```rust
// scripts/allocate-market/src/main.rs:56–58
let rent_lamports = client
    .get_minimum_balance_for_rent_exemption(Market::LEN)
    .context("fetch rent-exempt minimum")?;
```

なぜ往復するのか。理由は、`Rent` が**sysvar** だからだ。値はバイナリにハードコードされておらず、オンチェーンで管理される。将来のランタイム更新で（原理的には）変更されうる。「このクラスタで実際に強制される値」を知る唯一の方法が RPC への問い合わせである。

**SDK が隠していること:** Anchor の `#[account(init, ..., space = 8 + 248)]` は `space` 引数を読み取り、**プログラム内で** `Rent::get()?.minimum_balance(space)` を呼ぶ（オンチェーン sysvar にアクセスできるので RPC は不要）。その値を `create_account` の lamport 量に渡す。`space = 8 + ...` の `8` は Anchor 自身のディスクリミネータ用オーバーヘッドだ。Anchor は型を実行時に識別するため、全アカウントの先頭に 8 バイトを必ず付ける。本書の `Market` はオフセット 0 に独自のディスクリミネータを持っており、同じ 8 バイトの課金を払っている — ただし、それが目に見える形で。

> **演習 §1.2.** 0 バイトのアカウント、256 バイトのアカウント、10,000 バイトのアカウント（Anchor が推奨する上限）について、`minimum_balance` を手計算せよ。次にクラスタで照合する。
> ```
> solana rent <bytes>
> ```
> 128 バイトのオーバーヘッドは実際にはどこに格納されているのか。（ヒント: `lib.rs:67–70` のフィールド名コメントを読むこと。）

---

## §1.3  System プログラムからアカウントを確保する

System プログラム（`11111111111111111111111111111111`）は、アカウントを存在させることができる唯一のプログラムである。System はすべてのウォレットを所有し、lamport を自由に動かせる唯一の存在であり、他のすべてのプログラムが最初のアカウントを得る出発点になる。

`solana-system-interface-1.0.0/src/instruction.rs:80–95` を開こう。

```rust
pub enum SystemInstruction {
    /// Create a new account
    ///
    /// # Account references
    ///   0. `[WRITE, SIGNER]` Funding account
    ///   1. `[WRITE, SIGNER]` New account
    CreateAccount {
        /// Number of lamports to transfer to the new account
        lamports: u64,
        /// Number of bytes of memory to allocate
        space: u64,
        /// Address of program that will own the new account
        owner: Pubkey,
    },
    // ...
}
```

`CreateAccount` は 1 回のシステムコールで 3 つのことを行う。

1. 資金口座から新アカウントへ `lamports` を**送金**する。
2. 新アカウントの `data` フィールドに `space` バイトを**確保**する。
3. 新アカウントの所有者として `owner` を**割り当てる**。

`instruction.rs:9–12` のドキュメントコメントも同じ分解を明記している。

> Account creation typically involves three steps: `allocate` space, `transfer` lamports for rent, `assign` to its owning program. The `create_account` function does all three at once.

呼び出すコンストラクタは `instruction.rs:406–426` にある。

```rust
pub fn create_account(
    from_pubkey: &Pubkey,
    to_pubkey: &Pubkey,
    lamports: u64,
    space: u64,
    owner: &Pubkey,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*from_pubkey, true),
        AccountMeta::new(*to_pubkey, true),
    ];
    // ...
}
```

両方の `AccountMeta::new` に `true` が付いている点に注目。この `true` は**署名者の要求**を意味する。資金口座と新アカウントの両方が、このトランザクションに署名しなければならない。これは初めて見ると違和感がある。まだ存在しないアカウントが、なぜ署名する必要があるのか。

答えは「乗っ取り防止」だ。もし支払者だけが署名すればよいなら、誰かが 0.003 SOL を支払って**あなたのアドレス**にアカウントを作成し、所有者を**相手が管理するプログラム**に設定し、本来あなたが到達する前にそのアドレスをゴミに縛り付けることができてしまう。新アカウントにも署名を要求することで、ランタイムは「新アドレスの秘密鍵を本当に持っている」ことを証明させる。lamport は支払者から出る。新アカウントの署名は、そのアカウントを名乗る鍵ペアを所有する者から出る。

本書のスクリプトでは `scripts/allocate-market/src/main.rs:79–85` がこれを行う。

```rust
let blockhash = client.get_latest_blockhash().context("fetch blockhash")?;
let tx = Transaction::new_signed_with_payer(
    &[create_ix],
    Some(&payer.pubkey()),
    &[&payer, &market],
    blockhash,
);
```

`&[&payer, &market]` — 署名者 2 名。`market` は 52 行目で `Keypair::new()` によりローカル生成した鍵ペアだ。その秘密鍵をこのトランザクション 1 回ぶんだけ手元に置いて署名する。それ以降は二度と使わない。アカウントは公開鍵で識別され、このトランザクション以降、何かを変更できるのは**所有プログラム**（今は System）だけになる。

第 3 章では、この `Keypair::new()` を Program-Derived Address (PDA) に置き換える。そこでは新アカウントの署名をプログラム自身が `invoke_signed` 経由で供給するようになる。モデルは同じ、署名する主体だけが変わる。

**SDK が隠していること:** Anchor の `#[account(init, payer = payer, space = ...)]` はおよそ次のように展開される。

```rust
// 1. レント免除残高を実行時に Rent::get() で計算する
let rent = Rent::get()?.minimum_balance(space);

// 2. System の CreateAccount 命令を組み立てる
let ix = system_instruction::create_account(
    payer.key,
    account.key,
    rent,
    space as u64,
    program_id,                   // <-- System ではなく自前のプログラム!
);

// 3. account が PDA なら invoke_signed、そうでなければ invoke で CPI 発行
invoke_signed(&ix, &[payer, account, system_program], &[seeds_with_bump])?;

// 4. Anchor の 8 バイトディスクリミネータを data[0..8] に書く
account.try_borrow_mut_data()?[..8].copy_from_slice(&MyType::DISCRIMINATOR);

// 5. data の残りをゼロ初期化する（Anchor のアカウント型は repr(C) Pod 風なので）
```

5 ステップ。属性 1 行の裏に。どれも誤りではない — Anchor の選択は妥当な既定値だ — が、どれもあなたが下した決定ではない。

> **演習 §1.3.** 新アカウントの所有者は `create_account` を呼んだプログラムが指定する。本書のスクリプトでは、新アカウントの所有者は何になるか。`main.rs:69` と `main.rs:71–77` を見よ。第 1 章としてはなぜそれが正しい選択なのか。

---

## §1.4  バイトを読む

スクリプトの最後の仕事は、作成したアカウントを取得してダンプすることだ。ダンプの表は `main.rs:113–125` にある。

```rust
fn dump_market_bytes(data: &[u8]) {
    let regions: &[(usize, usize, &str)] = &[
        (0, 8, "discriminator      [u8; 8]    expected: MARKET\\0\\0"),
        (8, 1, "version            u8"),
        (9, 1, "bump               u8"),
        (10, 6, "_pad0              [u8; 6]"),
        (16, 32, "authority          [u8; 32]"),
        (48, 32, "base_mint          [u8; 32]"),
        (80, 32, "quote_mint         [u8; 32]"),
        (112, 8, "tick_size          u64"),
        (120, 8, "lot_size           u64"),
        (128, 128, "_reserved          [u8; 128]"),
    ];
    // ...
}
```

これらのオフセットは捏造ではない。`crates/state/src/lib.rs:43–56` の `Market` 構造体定義からそのまま導かれる。

```rust
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Market {
    pub discriminator: [u8; 8],   // 0..8
    pub version: u8,              // 8
    pub bump: u8,                 // 9
    pub _pad0: [u8; 6],           // 10..16
    pub authority: [u8; 32],      // 16..48
    pub base_mint: [u8; 32],      // 48..80
    pub quote_mint: [u8; 32],     // 80..112
    pub tick_size: u64,           // 112..120
    pub lot_size: u64,            // 120..128
    pub _reserved: [u8; 128],     // 128..256
}
```

2 点だけ立ち止まりたい。

**`_pad0: [u8; 6]` フィールドの理由。** オフセット 9 の `bump: u8` の次は `authority: [u8; 32]` だ。`[u8; 32]` のアラインメント要求は 1 だが、本構造体は `#[repr(C)]` で、後続の `tick_size: u64` を 8 バイト境界に置きたい — 将来のコードがアラインなしアクセスのリスクなく `u64` として読めるように。この 6 バイトのパディングがあることで、次の 8 バイト境界が 10 ではなく 16 に来る。コンパイラが暗黙に挿入するパディングに任せず、明示的に宣言したのには理由がある。隠れたパディングは `bytemuck::Pod` 要件を壊す。`Pod` は構造体の全バイトが初期化済みで可視であることを要求するからだ。

**`Pubkey` を `[u8; 32]` として持つ理由。** `crates/state/src/lib.rs:8–15` のドキュメントコメントが説明している。`solana_program::pubkey::Pubkey` は `bytemuck::Pod` を上流で実装していない。レイアウトを `Pod` 安全に保つには、生の 32 バイトとして保持するしかない。これは教育的にも誠実だ。`Pubkey` は実体としては 32 バイトの並びでしかなく、型名はその並びに名前を付けたものに過ぎない。第 2 章のプログラムは境界で型変換する。

新しい `solana-test-validator` 上でスクリプトを実行すると、期待される出力はおよそ次のようになる。

```
rpc:            http://127.0.0.1:8899
payer:          7c5...QJZ
market pubkey:  E2k...A9M
space:          256 bytes
rent lamports:  2672640  (0.002673 SOL)

create_account signature: 5xH...t8N

account metadata:
  owner:        11111111111111111111111111111111
  lamports:     2672640
  executable:   false
  rent_epoch:   18446744073709551615
  data length:  256

account data (raw bytes, annotated against openhl_state::Market):

  0x0000  discriminator      [u8; 8]    expected: MARKET\0\0
          00 00 00 00 00 00 00 00
  0x0008  version            u8
          00
  0x0009  bump               u8
          00
  0x000a  _pad0              [u8; 6]
          00 00 00 00 00 00
  0x0010  authority          [u8; 32]
          00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
          00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
  ... (256 バイトすべてゼロ)
```

注目したい点はいくつかある。

- `owner: 11111111111111111111111111111111` — base58 で 1 が並ぶアドレスは System プログラムを表す。`main.rs:69` で明示的にそう指定した。
- `lamports: 2672640` — §1.2 で計算したレント免除最低額そのもの。多くも少なくもない。
- `executable: false` — これはデータアカウントであってプログラムではない。
- `rent_epoch: 18446744073709551615` — これは `u64::MAX` だ。ランタイムはレント免除アカウントをこの番兵値で印付け、「このアカウントには課金しない」を実質的に表現する。これが現在のレント体系の遺物の姿だ。
- `data: [0u8; 256]` — 全バイトゼロ。書き込んだことがないからだ。System プログラムは自分が所有するデータフィールドへの書き込みを誰にも許可しない。アロケータがゼロ初期化するので、バイトはゼロのまま残る。

オフセット 0 のディスクリミネータは `00 00 00 00 00 00 00 00` だ。`4d 41 52 4b 45 54 00 00`（"MARKET\0\0"）ではない。正しいバイト数を確保したが、`MARKET_DISCRIMINATOR` を書き込んだ者はまだいない。それが第 2 章の仕事である。

**SDK が隠していること:** Anchor の型付き `Account<'info, T>` インターフェース経由でアカウントを取得すると、フレームワークは `data[0..8]` を読み、`T::DISCRIMINATOR` と**比較**する。一致しなければ、コードがアカウントに触れる前にエラーが返る。これは非常に有用な安全装置だ — 同時に、いま作成した状態（正しいサイズ、全ゼロ）のアカウントは Anchor のディスクリミネータチェックを通らず、型付きコードからは存在しないように見える。生のバイトは確かにそこにある。Anchor がただ見ないだけだ。

> **演習 §1.4.** スクリプトを `solana-test-validator` に対して実行せよ。別端末で、スクリプトが出力した market pubkey に対し `solana account <market_pubkey>` を実行し、スクリプトの出力と突き合わせよ。全フィールドで一致するはずだ。`solana account` が表示してスクリプトが表示しない情報を 1 つ、スクリプトが表示して `solana account` が表示しない情報を 1 つ、それぞれ見つけよ。

---

## §1.5  `#[account(init, ...)]` の中身

ここまでで、Anchor の最も典型的な属性が肩代わりしていたことをすべて見た。突き合わせて並べると次のとおりだ。

| Anchor がやること | 平文に展開すると |
|---|---|
| 属性から `space = N` を読む | `let space = N;` |
| `Rent::get()?.minimum_balance(space)` を呼ぶ | §1.2 — 本書のスクリプトは RPC 経由の `get_minimum_balance_for_rent_exemption` を使う |
| `system_instruction::create_account` を組み立てる | §1.3 — `main.rs:71–77` |
| `invoke_signed`（PDA シード付き）または `invoke` で発行する | 本書のスクリプトでは省略。クライアント側で署名している。CPI は第 2 章で導入する |
| `T::DISCRIMINATOR` を `data[0..8]` に書く | 省略。§1.4 が示すとおりバイトはゼロのまま |
| 新アカウントの所有者をプログラム ID にする | 意図的に `system_program::ID` を指定 |
| 型付きアカウントビューを Rust 構造体に束縛する | 生の `account.data: Vec<u8>` と独立した `Market` 構造体を使う |

マクロは本物の仕事をしている。「ただの糖衣」ではない。マクロは各ステップで既定値を選ぶ — 支払者の選定、レント計算、所有者 = プログラム ID、ディスクリミネータ = 型 ID、レイアウト = Borsh 風 — そして、**ほとんどの場合**それで正しい。誤っているとき（プログラム横断の所有権、独自ディスクリミネータ、ZK 検証器向けのバイト厳密レイアウト）には、展開のどの行を上書きすべきかを知っている必要がある。

抽象化は内側から 3 層で積まれている。

1. **`solana-program` のシステムコール** — ランタイム ABI。`sol_invoke`、`sol_log`、`create_program_address`。最も金属に近い。直接呼ぶことはまれだが、上層の抽象すべてが最終的にここを通る。
2. **`solana-sdk` のラッパ** — `Transaction::new_signed_with_payer`、`Account`、`Rent`、命令コンストラクタ群。人間工学的で、型付きで、魔法は使わない。
3. **`anchor-lang` のマクロ** — `#[program]`、`#[derive(Accounts)]`、`#[account(...)]`。人間工学を最大化し、意見の強い既定値を持ち、定型コードを生成する。最も深い抽象。

本トラックが教えるのは下 2 層だ。それを理解していれば、第 3 層の既定値があなたを裏切ったとき、デバッグできる。

---

## §1.6  まとめと自己検証

### まとめ図

```
┌─────────────────────────────────────────────────────────────────┐
│                       Solana アカウント                          │
│                                                                 │
│   lamports     : u64       ← 減算できるのは所有プログラムのみ    │
│   data         : Vec<u8>   ← 書き込めるのは所有プログラムのみ    │
│   owner        : Pubkey    ← 作成時に設定、Assign で変更         │
│   executable   : bool      ← 単方向: false → true 後は永久に RO  │
│   rent_epoch   : u64       ← 遺物。u64::MAX = レント免除         │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘

アカウント作成（System プログラム）:

  payer ──署名──┐
                ▼
         CreateAccount { lamports, space, owner }
                │
                ▼
  new ──署名────┤  ← 署名が鍵ペア所有の証明になる
                ▼
       ランタイム: `space` バイトをゼロで確保し、
                   payer から new に `lamports` を送金し、
                   new.owner = `owner` に設定する
```

### 自分で検証する 3 項目

1. **所有者の確認。** スクリプト実行後、`solana account <market_pubkey>` を実行する。`Owner` 行が `11111111111111111111111111111111`（System）であることを確認せよ。本書のスクリプトはこれを [`main.rs:69`](../../scripts/allocate-market/src/main.rs#L69) で明示的に要求し、ランタイムはそれに従う。
2. **レント免除計算。** `solana rent 256` を実行する。出力はスクリプトが表示した `rent lamports` と一致するはずだ。両者は同じ計算式 [`solana-rent-2.2.1/src/lib.rs:93`](#) を出所とする — 片方はローカル計算、もう片方は RPC 経由。
3. **レイアウトのオフセット。** [`crates/state/src/lib.rs:43–56`](../../crates/state/src/lib.rs#L43) を開き、フィールド長を手で足し合わせよ。`quote_mint` がバイト 80 から始まり、`_reserved` がバイト 256 で終わることを確認すること。`cargo test -p openhl-state` を実行すれば、コンパイラもこれを保証する — [`lib.rs:67–70`](../../crates/state/src/lib.rs#L67) の `market_size_is_256_bytes` テストは、フィールドのサイズが変わった瞬間に失敗する。

---

## 第 2 章への導線

オンチェーンにアカウントを 1 つ持った。5 つのフィールドの中身を正確に把握している。データフィールドは 256 バイトのゼロで、System プログラムがその所有者であり、System には任意データを書き込む命令がないため — そのバイトは所有者が変わらない限り永久にゼロのままである。

所有者を奪い取るには、自前の Solana プログラムが要る。Anchor プログラムではない。`entrypoint!`、`&[AccountInfo]`、手書きの命令ディスパッチャから組み立てたプログラムだ。第 2 章ではこのプログラムを構築し、同じバリデータにデプロイし、これを使って (a) アカウントの所有権を取り、(b) `MARKET_DISCRIMINATOR` のバイトをオフセット 0 に書き込む。

第 2 章のスクリプトが終わったとき、同じ 16 進ダンプの先頭は `00 00 00 00 00 00 00 00` ではなく `4d 41 52 4b 45 54 00 00` になっている。その 8 バイトの違いが、初めて作る自前プログラムの全可視成果だ — そして不可視の成果は、これからどんな Anchor プログラムがアカウントをデシリアライズするのを見ても、その瞬間に何が起きているか理解できるようになっていることである。
