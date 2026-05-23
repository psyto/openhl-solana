# 第2章 — Anchor を使わずネイティブプログラムを書く

> 状態: ドラフト (v0.1)。
> 教材コード: [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)、[`scripts/init-market/src/main.rs`](../../scripts/init-market/src/main.rs)。
> 検証対象バージョン: solana-program 2.3.0、solana-program-entrypoint 2.3.0、solana-account-info 2.3.0、solana-program-error 2.2.2、solana-system-interface 1.0.0。

---

## §2.0  はじめに

第 1 章の終わりに、オンチェーンアカウントを 1 つ手にした。256 バイトのゼロ、所有者は System プログラム、しかし 1 ビットも書き換える手段がない — System には「任意のデータを書き込む」命令が存在しないからだ。あのアカウントは器だった。次の段階は、その中身を満たせる唯一の存在を作ることだ。

それが Solana プログラムである。ただし Anchor プログラムではない。本章では、プログラム全体を手で書く。`lib.rs` が 1 つ。`#[derive(Accounts)]` なし。`#[program]` なし。Borsh なし。あるのは `entrypoint!`、`&[AccountInfo]`、手作業のバイト復号、`bytemuck` キャストだけだ。

次の順に進める。

1. `solana-program-entrypoint` を開き、`entrypoint!(process_instruction)` が実際に展開するものを見る。
2. `solana-account-info` を開き、第 1 章の `Account` には無く `AccountInfo` だけが持つものを確認する。
3. `programs/openhl-core/src/lib.rs` を 1 行ずつ歩く — ディスパッチャ、所有者チェック、bytemuck キャスト。
4. `scripts/init-market/src/main.rs` を 1 行ずつ歩く — クライアント側の単一トランザクションに 2 つの命令、`System::Assign` の後に `openhl-core::Initialize`。
5. 実行してアカウントを 16 進ダンプし、`[0..8]` のバイトが `00 00 00 00 00 00 00 00` から `4d 41 52 4b 45 54 00 00` に切り替わる瞬間を観察する — 初めて自前の Solana プログラムを書いた 8 バイト分の対価である。
6. これらを `#[program]` + `#[derive(Accounts)]` がどう肩代わりしていたかを、責務ごとに対応づけて列挙する。

終えるころには、任意の Anchor プログラムの `cargo expand` 出力を読み、生成された関数のどれが本章で自分が書いたどの行に対応するかを特定できるようになっている。コストは Rust 約 160 行に集中して向き合うこと。利益は永続する。

---

## §2.1  `entrypoint!` と `process_instruction` — プログラムの ABI

Solana プログラムはすべて `.so` ファイルで、唯一の C エクスポート関数 `entrypoint` を持つ。Solana ローダがこの関数を呼び、シリアライズされたバッファへのポインタを渡す。バッファにはプログラム ID、アカウント群、命令データが入っている。`entrypoint!` マクロはこの ABI を Rust 流のファサードで包む。

`solana-program-entrypoint-2.3.0/src/lib.rs:127–142` を開こう。

```rust
#[macro_export]
macro_rules! entrypoint {
    ($process_instruction:ident) => {
        /// # Safety
        #[no_mangle]
        pub unsafe extern "C" fn entrypoint(input: *mut u8) -> u64 {
            let (program_id, accounts, instruction_data) = unsafe { $crate::deserialize(input) };
            match $process_instruction(program_id, &accounts, instruction_data) {
                Ok(()) => $crate::SUCCESS,
                Err(error) => error.into(),
            }
        }
        $crate::custom_heap_default!();
        $crate::custom_panic_default!();
    };
}
```

15 行。マクロの全体だ。やっていることは 4 つ。

1. `no_mangle extern "C" entrypoint` 関数をエクスポートし、ローダが名前で見つけられるようにする。
2. `$crate::deserialize(input)` を呼んで、ローダのバイナリ入力を `(program_id, accounts, instruction_data)` に分解する — それぞれ `&Pubkey`、`Vec<AccountInfo>`、`&[u8]`。
3. その 3 つを**あなたの**関数（マクロ引数に渡した識別子）に転送する。
4. あなたの `Result<(), ProgramError>` を、ローダが期待する `u64` 終了コードに変換する（成功なら `0`、失敗ならエンコードされたエラー）。

これで全部だ。ルーターもミドルウェアも拡張ポイントもない。`entrypoint!($fn)` に渡した Rust 関数 1 つだけが、チェーン全体と自分のプログラムが対話する唯一の地点である。

本書の呼び出しは `programs/openhl-core/src/lib.rs:25–26` にある。

```rust
#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);
```

`cfg` ゲートにより、ホスト側バイナリ（クライアントやテスト、要するに型は欲しいが BPF エントリポイントは要らないコード）にこのクレートをリンクするときはエントリポイントを外せる。`init-market` クレートは `no-entrypoint` 機能を有効にしているので、プログラムの型はリンクされるが BPF エントリポイントは含まれない。クライアント自身の `main` と名前衝突するのを防ぐためだ。

本書の `process_instruction` は標準的な署名に従う。`lib.rs:33–37`。

```rust
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
```

この署名は `entrypoint!` によって固定されている。パラメータを増やすこともできず、別の引数型を取ることもできず、別の戻り値型を返すこともできない。`ProgramResult` は `Result<(), ProgramError>` の別名にすぎない — `solana-program-error-2.2.2/src/lib.rs:28` を見よ。

```rust
pub type ProgramResult = std::result::Result<(), ProgramError>;
```

`ProgramError` 自体は `src/lib.rs:33–63` の 24 バリアントの enum で、標準的な失敗モードを網羅する。`IncorrectProgramId`、`NotEnoughAccountKeys`、`InvalidAccountData`、`AccountAlreadyInitialized` など。プログラム固有のエラーを返したい場合は `ProgramError::Custom(u32)` で独自の数値コードを使える。

**Anchor が隠していること:** Anchor の `#[program]` マクロは、まさにこの署名の関数を生成する。Anchor で書く「インストラクション」 — ハンドラ関数群 — はエントリポイントではない。Anchor が生成した独自の `process_instruction` が命令データを解析し、ディスクリミネータを引き、アカウントをデシリアライズし、正しいハンドラへ振り分けた**後**にディスパッチされる関数である。マクロが生成するためそのコードは目に入らない。しかし確かに存在しており、形はいま書いたものと同じだ。

> **演習 §2.1.** `cargo build-sbf --manifest-path programs/openhl-core/Cargo.toml` でプログラムをビルドせよ。`target/deploy/openhl_core.so` を `nm`（または `objdump`）で調べ、エクスポートされた `entrypoint` シンボルを見つけよ。外部リンケージを持つ `T`（テキスト/コード）シンボルがそれだけであることを確認すること。

---

## §2.2  `AccountInfo` — プログラムが実際に見ているもの

第 1 章では `Account` を扱った。`RpcClient::get_account` が返す型で、`data: Vec<u8>` を所有していた。オンチェーンでは別の型が見える — `AccountInfo` だ。

`solana-account-info-2.3.0/src/lib.rs:19–39` を開こう。

```rust
/// Account information
#[derive(Clone)]
#[repr(C)]
pub struct AccountInfo<'a> {
    /// Public key of the account
    pub key: &'a Pubkey,
    /// The lamports in the account.  Modifiable by programs.
    pub lamports: Rc<RefCell<&'a mut u64>>,
    /// The data held in this account.  Modifiable by programs.
    pub data: Rc<RefCell<&'a mut [u8]>>,
    /// Program that owns this account
    pub owner: &'a Pubkey,
    /// The epoch at which this account will next owe rent
    pub rent_epoch: u64,
    /// Was the transaction signed by this account's public key?
    pub is_signer: bool,
    /// Is the account writable?
    pub is_writable: bool,
    /// This account's data contains a loaded program (and is now read-only)
    pub executable: bool,
}
```

第 1 章の `Account`（`solana-account-2.2.1/src/lib.rs:44–56`）と並べて見よう。

| `Account` | `AccountInfo` |
|---|---|
| `lamports: u64` | `lamports: Rc<RefCell<&'a mut u64>>` |
| `data: Vec<u8>` | `data: Rc<RefCell<&'a mut [u8]>>` |
| `owner: Pubkey` | `owner: &'a Pubkey` |
| `executable: bool` | `executable: bool` |
| `rent_epoch: Epoch` | `rent_epoch: u64` |
| — | `key: &'a Pubkey` |
| — | `is_signer: bool` |
| — | `is_writable: bool` |

2 つ変わり、3 つ増えた。

**変わった点**は所有権と可変性についてだ。ローダはあなたのプログラムに「ビュー」を渡す — ローダ自身が管理するバッファへの参照だ。プログラムは lamport 残高もデータバイトも所有しない。それは他人のメモリで、あなたは `Rc<RefCell<&mut _>>` の借用を受け取る。同一トランザクション内の複数命令から同じアカウントへ別々の `AccountInfo` 参照が共存できるように、かつ借用規則を実行時に強制できるようにするためだ。`Rc` がいるのは、同じ pubkey に対する `AccountInfo` をローダが複数命令にわたり同じインスタンスとして渡すから。`RefCell` がいるのは、この状況で Rust の借用チェッカが静的に借用規則を証明できないからである。

**増えた点**は、`Account` には存在しない実行時専用情報だ。

- **`key`** — アカウント自身の pubkey。`Account` は自身の pubkey を持たない。pubkey はオンチェーンのアカウントマップへのインデックスとして外側にある。`AccountInfo` は持っている。プログラムは pubkey から計算する場面が日常的だからだ（PDA 派生、アカウント→プログラムのマッピング）。
- **`is_signer`** — このアカウントが**外側のトランザクション**に署名したか。ランタイムが `AccountInfo` ごと、命令ごとに設定する。
- **`is_writable`** — トランザクションがこのアカウントを書き込み可能と宣言したか。たとえプログラム的に書ける条件が揃っていても（所有者が自分、サイズも十分）、トランザクションが書き込み可能と宣言していなければ、書き込みは確定時に失敗する。

この 3 つは、いま走っているトランザクション「について」ランタイムがプログラムに伝える手段だ。あなたが設定したのではない。ローダが設定する。

**Anchor が隠していること:** Anchor の型付きアカウントラッパ（`Account<'info, T>`、`Signer<'info>`、`UncheckedAccount<'info>` など）はすべて、内部に `AccountInfo` を持つ — `to_account_info()` で取り出せる。ラッパは型レベルのチェック（デシリアライズ、署名要求など）を上に重ねるが、下にある値は同じ `AccountInfo` だ。Anchor コードで `let info = ctx.accounts.market.to_account_info();` を見たら、それは抽象化を通り抜けて、いま本書が直接扱っている層に手を伸ばしている瞬間である。

> **演習 §2.2.** `process_initialize`（`lib.rs:79–81`）では `accounts.first()` で市場アカウントを取得しているが、`is_writable` をチェックしていない。なぜか。（ヒント: 書き込み不可のアカウントが渡された状態で `try_borrow_mut_data` を呼んだら、ランタイムはどのエラーコードを返すか。）

---

## §2.3  所有者チェック — プログラムで最も重要な 1 行

アカウントを 1 つ受け取って「これは market だ」と扱う。しかし**それが本物の自分の market であって、たとえば形だけ似たレント免除済み 256 バイトアカウントを誰かがでっち上げたものではない**と、どうやって知るのか。

答えは — 唯一の答えは — **所有者チェック**である。`programs/openhl-core/src/lib.rs:83–94` から。

```rust
// (1) Owner check. The single most-skipped check in Solana programs,
// and the source of most "but I checked the pubkey!" exploits. The
// *only* thing that proves an account is one of ours is that we own
// it. If owner is something else, the bytes inside could mean anything.
if market_ai.owner != program_id {
    msg!(
        "initialize: market owner {} != program {}",
        market_ai.owner,
        program_id
    );
    return Err(ProgramError::IncorrectProgramId);
}
```

第 1 章で確認したランタイム保証はこうだった。アカウントの `data` を書き換えられるのは所有プログラムだけ。対偶を取ると、`owner == program_id` なら、そのバイトを書き込めたのは**自分たち**だけになる。オフセット 0 のディスクリミネータは、ゼロ（アカウントは存在するが未初期化）か `MARKET_DISCRIMINATOR`（自分たちが初期化済み）のいずれかである。それ以外の値にはなりえない。なぜなら、書ける主体が他にいないからだ。

このチェックがなければ、攻撃者は次のことができてしまう。

1. **攻撃者自身**のプログラムが所有する 256 バイトアカウントを確保する。
2. `data[0..256]` に任意のバイトを書く。
3. それを本書の `Initialize` に渡す。
4. 他のチェックはすべて通る（サイズは 256、ディスクリミネータは攻撃者が設定したもの、ペイロードは正しく復号できる）。
5. プログラムは喜んでバイトを上書きする — しかし、**次回**ディスクリミネータをチェックしたとき見えるのは、自分が書いた値ではなく攻撃者が設定した値だ。さらに悪いことに、後の章で同じアカウントが `place_order` に渡されたとき、そのバイトを暗黙に信頼してしまう。

所有者チェックこそが「正しい形の 256 バイト」を「自分が書いた 256 バイト」に変える。これを省くことは、セキュリティモデルを省くことに等しい。

サイズチェック（`lib.rs:99–106`）は次に来るが、機械的だ。`bytemuck::from_bytes_mut::<Market>(buf)` はバッファが小さすぎるとパニックするので、明示的に拒否して綺麗なエラーコードを返す。初期化済みチェック（`lib.rs:111–117`）は 3 番目。ディスクリミネータが非ゼロなら、そのアカウントはすでに生きた `Market` なので、踏み潰してはならない。

**Anchor が隠していること:** Anchor の `#[account(mut)]` 制約と型付きラッパ `Account<'info, T>` が、所有者チェックを代行している。具体的には、Anchor が `Account<'info, MyType>` をデシリアライズする際、型付きビューを返す前に `account.owner == program_id` をアサートする。失敗すれば、自分のハンドラは呼ばれない。これは「覚えておけ」より確かに安全だ — しかし同時に、多くの Anchor 開発者が**なぜ**そのチェックが存在するかを身体化しないまま済んでしまう。自分の Anchor コードを開いて、所有者チェックを探してみよ。確かにそこにある。ただ、目に見えていないだけだ。

> **演習 §2.3.** `process_initialize` を編集して、所有者チェックを意図的にスキップせよ（87–94 行をコメントアウト）。ビルドし直し、第 1 章のアロケータが作った System 所有アカウント（§2.5 で追加する Assign ステップを**経ずに**）を `Initialize` に渡すトランザクションを組み立てよ。実行すると何が起きるか。なぜか。

---

## §2.4  データを書く — `try_borrow_mut_data` + `bytemuck` キャスト

`AccountInfo::data` は `Rc<RefCell<&'a mut [u8]>>` だ。書き込み可能なスライスを取り出すには `try_borrow_mut_data()` を呼ぶ。`programs/openhl-core/src/lib.rs:147–148`。

```rust
let mut data = market_ai.try_borrow_mut_data()?;
let market: &mut Market = bytemuck::from_bytes_mut(&mut data[..Market::LEN]);
```

操作は 2 つ。

1. **`try_borrow_mut_data()`** — 失敗しうる。`RefCell` がすでに借用されている可能性があるからだ。失敗ケースは `ProgramError::AccountBorrowFailed`。同じ `AccountInfo` がコールスタックのどこか別の場所で可変借用されている場合に発生する（たとえば同じアカウントで再入する CPI ハンドラなど）。本書のような末端の書き込みなら実務上失敗しない — それでも `?` を付けておけば、将来この箇所が二重借用される使い方をされても正しく動く。

2. **`bytemuck::from_bytes_mut::<Market>(buf)`** — `&mut [u8]` から `&mut Market` へのポインタキャスト。安全なのは、`Market` が `Pod` だからだ。全ビットが有効、パディングなし、`repr(C)`。サイズチェック（§2.3）でバッファがちょうど `Market::LEN` バイトであることを確認済みなので、キャストは well-defined である。`&mut [u8]` が同じバイト列を指す `&mut Market` ビューになる。コピーなし、確保なし、純粋な型再解釈である。

キャストが済んだら、フィールドごとに名前で書き込む（`lib.rs:150–159`）。

```rust
market.discriminator = MARKET_DISCRIMINATOR;
market.version = Market::VERSION;
market.bump = 0;
market._pad0 = [0u8; 6];
market.authority = authority;
market.base_mint = base_mint;
market.quote_mint = quote_mint;
market.tick_size = tick_size;
market.lot_size = lot_size;
market._reserved = [0u8; 128];
```

書き込みはアカウントのデータバッファ「上で」直接行われる。「保存」呼び出しはない。`process_instruction` が `Ok(())` を返した時点で、ローダは変更済みのバイトを読み取り、トランザクションの一部として元帳に確定する。

`_pad0` と `_reserved` のゼロ書き込みが明示的にあることに注意。必要はない（System のアロケータでバイトはすでにゼロ、bytemuck はパディングを追加しない — 構造体側で明示宣言したから）。しかし書いておくと、もしこのアカウントが過去の状態で非ゼロパディングを持つ形で再利用された場合に、コードがロバストになる。新規アカウントには過剰防衛だが、`realloc` されたアカウントなら効いてくる。

**Anchor が隠していること:** Anchor の型付きラッパは `account.fieldname = value;` を直接書ける。`try_borrow_mut_data` を呼ぶ必要はない。ラッパは内部で `RefMut` を保持し、`Drop` 時にアカウントへフラッシュする。さらに `init` 時には 8 バイトディスクリミネータも書き込んでくれる — その代償として、全アカウントが Anchor 独自のディスクリミネータ形式（`sha256("account:TypeName")` の先頭 8 バイト）を持つことになる。本書の `MARKET\0\0` のように人間が読めるものではない。

> **演習 §2.4.** `market.discriminator = MARKET_DISCRIMINATOR;` を、オフセット 0 に 1 バイトだけ書き込む形に変えよ（例: `data[0] = 0x42;`）。次に `init-market` を実行したとき、どのエラーコードが返るか。なぜ破損ではなくそのエラーなのか。

---

## §2.5  クライアント側 — `Assign` + `Initialize` を 1 トランザクションで

第 1 章で作ったアカウントの所有者は System だ。本書のプログラムはまだ書き込めない — 所有者チェックが落ちる。所有権を奪うには `System::Assign` 命令が必要で、市場アカウント自身の鍵ペアで署名する必要がある。

`scripts/init-market/src/main.rs:117–146` から。

```rust
// (1) System::Assign — transfer ownership to our program.
let assign_ix = system_instruction::assign(&market.pubkey(), &program_id);

// (2) openhl-core::Initialize — see programs/openhl-core/src/lib.rs.
let mut init_data = Vec::with_capacity(1 + 32 + 32 + 32 + 8 + 8);
init_data.push(0u8); // tag = Initialize
init_data.extend_from_slice(authority.as_ref());
init_data.extend_from_slice(base_mint.as_ref());
init_data.extend_from_slice(quote_mint.as_ref());
init_data.extend_from_slice(&cli.tick_size.to_le_bytes());
init_data.extend_from_slice(&cli.lot_size.to_le_bytes());

let init_ix = Instruction {
    program_id,
    accounts: vec![AccountMeta::new(market.pubkey(), false)],
    data: init_data,
};

let blockhash = client.get_latest_blockhash().context("fetch blockhash")?;
let tx = Transaction::new_signed_with_payer(
    &[assign_ix, init_ix],
    Some(&payer.pubkey()),
    &[&payer, &market],
    blockhash,
);
```

注目したい点は 3 つ。

**`Assign` が market の署名を要求する理由。** `solana-system-interface-1.0.0/src/instruction.rs:621–628` を開こう。

```rust
pub fn assign(pubkey: &Pubkey, owner: &Pubkey) -> Instruction {
    let account_metas = vec![AccountMeta::new(*pubkey, true)];
    // ...
}
```

`AccountMeta::new(*pubkey, true)` の `true` は**署名者の要求**を意味する。第 1 章の `CreateAccount` と同じだ。ランタイムは「このアカウントを現在管理している鍵ペアの所有者が、所有権変更に同意している」という証明を要求する。そうでなければ、誰でも任意のレント免除済み System 所有アカウントを、自分が管理するプログラムへ再割り当てして「奪う」ことができてしまう。

**2 つの命令を 1 トランザクションに収めた理由。** トランザクションはアトミックだ。すべての命令が確定するか、まったく何もしないかのどちらかだ。`[Assign, Initialize]` を 1 トランザクションに束ねることで、「openhl-core が未初期化の 256 バイトゼロ market を所有している」状態が観測されない。外側から見ると、アカウントは「System 所有 + ゼロデータ」から「openhl-core 所有 + 初期化済みデータ」へ直接遷移する。後の章では、この原子性が「半分初期化されたアカウントを誰かが読んでしまう」種類のバグから守ってくれる。

**Init 命令で `AccountMeta::new(market.pubkey(), false)` としている理由。** market アカウントは確かに書き込み可能だ（データを変更する）。しかし `Initialize` 命令への**署名**は不要だ。署名要求は命令単位であって、トランザクション単位ではない。Assign は market 鍵ペアの署名を要求する（System プログラムが強制する）が、Initialize は要求しない（本書のプログラムは所有者チェックだけを行い、署名者チェックはしない）。異なるセキュリティモデル、異なる `is_signer` フラグ、ということだ。

末尾の `&[&payer, &market]` はトランザクション単位の署名者リストだ。トランザクションは署名者を一度だけ集める。各命令の `AccountMeta` がその中のどれを必要とするかを宣言する。

> **演習 §2.5.** `solana-test-validator` に対して `init-market` を実行せよ（`openhl_core.so` をデプロイした後で）。同じ market アカウントに対して `init-market` をもう一度実行する。2 回目はどのエラーが返るか。さかのぼって `process_initialize` のどのチェックが拒否したかを特定せよ。

---

## §2.6  `#[program]` と `#[derive(Accounts)]` が実際に生成しているもの

第 1 章 §1.5 では `#[account(init, ...)]` の展開を歩いた。本章での等価物はさらに大きく、`#[program]` + `#[derive(Accounts)]` のペア全体だ。突き合わせて並べると次のとおり。

| Anchor がやること | 平文に展開すると |
|---|---|
| `entrypoint!` の呼び出しを生成する | `lib.rs:25–26` |
| 8 バイトディスクリミネータを復号する `process_instruction` を生成する | `lib.rs:38–48`（本書は 1 バイトタグを使う） |
| `#[program]` 関数 1 つにつき 1 つの match アームを生成し、ハンドラへディスパッチする | `lib.rs:42–48` |
| 型付き `Accounts` 構造体へアカウントをデシリアライズし、各制約（`#[account(mut)]`、`#[account(signer)]` など）を強制する | `lib.rs:79–117`（所有者チェック、サイズチェック、初期化済みチェック） |
| 全 `Account<'info, T>` について `account.owner == program_id` をアサートする | `lib.rs:87–94` |
| Borsh でハンドラ引数構造体に命令データをデシリアライズする | `lib.rs:119–135`（手作業バイト復号） |
| 型付き引数で自分のハンドラ関数を呼ぶ | `lib.rs:65–162`（本書のハンドラは `process_initialize`） |
| 変更済み `Account<'info, T>` を `Drop` 時にアカウントデータへシリアライズして戻す | `lib.rs:147–159`（本書は in-place で書く） |
| 戻された `Result<(), Error>` をローダの `u64` 終了コードに変換する | `entrypoint!` 自身から継承 |

責務は 8 つ。Anchor はマクロ生成ですべてを処理する。本書は約 130 行の Rust で同じことをした。どちらのアプローチも間違いではない。重要なのは、**8 つの責務がすべて存在する**という事実だ — マクロが隠してくれているだけで、消えてはいない。

Anchor プログラムが期待外の挙動を示したとき — 誤ったアカウントが渡された、署名者チェックが効かない、ディスクリミネータが衝突した、シリアライズ形式が想定外 — このリストを心の中で辿り直し、どのステップで何が起きたかを問うことでデバッグできる。リストを知っていることが、Anchor を自信を持ってデバッグすることと、推測でデバッグすることの差を生む。

---

## §2.7  まとめと自己検証

### まとめ図

```
トランザクション
┌────────────────────────────────────────────────────────────────┐
│                                                                │
│  命令[0]: System::Assign                                        │
│    accounts: [market (WRITE, SIGNER)]                          │
│    data:     SystemInstruction::Assign { owner: program_id }   │
│    effect:   market.owner: System → openhl_core                │
│                                                                │
│  命令[1]: openhl_core::Initialize                              │
│    accounts: [market (WRITE)]                                  │
│    data:     [tag=0, authority, base_mint, quote_mint,         │
│               tick_size, lot_size]                             │
│    effect:   market.data[0..256] = 初期化済み Market レイアウト│
│                                                                │
│  署名者: [payer, market]                                       │
│                                                                │
└────────────────────────────────────────────────────────────────┘
                              │
                              ▼
   アトミック確定: 両方の命令が適用されるか、どちらも適用されない
```

### 自分で検証する 3 項目

1. **ディスクリミネータが切り替わった。** `init-market` を実行した後、16 進ダンプの先頭は `4d 41 52 4b 45 54 00 00`（= "MARKET\0\0"）になっているはずだ。`init-market` を実行して確認せよ。第 1 章の全ゼロ出力と比べてみよ — この 8 バイトが、初めて Solana プログラムを書いた可視成果のすべてである。
2. **所有者が変わった。** `init-market` の後、`solana account <market_pubkey>` を実行せよ。`Owner` 行は、いまやデプロイ済みの `openhl-core` プログラム ID を示し、`11111111111111111111111111111111` ではないはずだ。`Assign` 命令がそれを行い、`Initialize` 命令はその結果に依存していた。
3. **再実行が拒否された。** 同じ market に対して `init-market` を 2 回目に実行せよ。トランザクションは失敗するはずだ。オンチェーンログ（`solana logs --include-failed`）を辿り、[`lib.rs:114`](../../programs/openhl-core/src/lib.rs#L114) からの `initialize: market already initialized` メッセージを見つけよ。[`lib.rs:111–117`](../../programs/openhl-core/src/lib.rs#L111) の初期化済みチェックが拒否したのだ。

---

## 第 3 章への導線

アカウントを作って所有することはできるようになった。しかし、まだ**プログラムからアドレスが導出されるアカウント**を作ることはできない。第 1 章と第 2 章で扱ったすべてのアカウントは、クライアント側で生成したアドホックな鍵ペアで識別されていた。1 アカウントのデモには通用するが、それ以外には通用しない。次回、特定の `(base_mint, quote_mint)` ペアに対する market アカウントを、鍵ペアをオフチェーンに保管せずどう見つけるのか。ユーザのポジションアカウントを、データベースでマッピングを追わずにユーザのウォレットへどう紐付けるのか。

答えは Program-Derived Address (PDA) だ。シードとプログラム ID から数学的に派生される pubkey で、対応する秘密鍵を持たない。第 3 章では派生を手で歩き、`invoke_signed` がどう「プログラムが所有する PDA のために」プログラム自身に署名させるかを示し、第 1 章の `Keypair::new()` を `find_program_address(&[b"market", base_mint.as_ref(), quote_mint.as_ref()], program_id)` の派生に置き換える。

第 3 章が終わったとき、同じ market は**予測可能な**アドレスに住んでいる。base mint と quote mint を知る任意のクライアントが、外部状態なしにアドレスを再計算できる — それこそが Solana プログラムをコンポーザブルにする本質である。
