# 第6章 — Vault Deposits で歩く CPI 内部

> 状態: ドラフト (v0.1)。
> 教材コード: [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（`process_create_vault` 615–728 行、`process_deposit` 731–800 行）、[`scripts/create-vault/src/main.rs`](../../scripts/create-vault/src/main.rs)、[`scripts/deposit/src/main.rs`](../../scripts/deposit/src/main.rs)。
> 検証対象バージョン: solana-cpi 2.2.1、solana-program 2.3.0、SPL Token プログラム（TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA）。

---

## Phase B prologue — 本カリキュラムが行うアーキテクチャ選択

Perp DEX は 2 通りに作れる。**既存チェーン上のプログラム** — マッチング、vault、ファンディング、清算ロジックをスマートコントラクトとして書き、コンセンサス・実行・バリデータ・ネットワーキング・ウォレットはチェーンに任せる。これが Phase B が組み立てる路線だ。**独自 L1** — 同じビジネスロジックを独自のコンセンサスエンジン・実行層・P2P ネットワーク・RPC と一緒にパッケージし、他のバリデータが走らせるバイナリを出荷する。

両者は同じビジネス問題（注文マッチ、担保保持、ファンディング支払い、水没ポジションの清算）を同じオンチェーン プリミティブで解く。それ以外のすべてが大きく分岐する。

**Solana 上のプログラム路線**（本カリキュラム）: 1 コマンドで deploy、既存流動性とウォレットを継承、既存バリデータのセキュリティに乗る。コードベースは 1 リポジトリに収まり、1 チームで出荷できる。ホストチェーンの slot 時間、コンピュートバジェット、バリデータ集中度に縛られる。値捕捉は部分的 — トランザクション価値の実質的な一部が自分の制御外のアクターに流れる。

**独自 L1 路線**: バリデータ集合の bootstrap に数ヶ月、自前のブリッジとステーブルコイン経済、自前のウォレット統合と上場交渉、自前の MEV ポリシー。コードベースは consensus + execution + networking + RPC にまたがって複数 crate に分散する。対価として、フルなランタイム制御（block time、順序、自前の負荷向け precompile）とフル値捕捉（バリデータ中間業者なし）が得られる。

L1 路線が必要になるのは、次の 3 条件が**揃って**成り立つときだ:

1. ホストチェーンが提供できないランタイム性質が必要 — sub-100ms 確認、独自マッチング precompile、決定論的な MEV-free 順序、EVM 互換でない精度での決済。
2. ビジネスモデルがトランザクション価値の完全捕捉に依存しており、ホストチェーンのバリデータ経済と分け合うわけにいかない。
3. チームと runway がチェーンを数年運営できる規模 — プログラム 1 つを deploy するのとは別次元の運用責任。

Hyperliquid は 3 条件すべてが揃った教科書的ケースだ。perp DEX プロジェクトの大半はどれも揃わない。そしてホストチェーン上のプログラム路線は、ローンチまでに掛かる時間・インフラ コスト・チーム規模のいずれにおいても、桁違いに安価だ。

本カリキュラムはプログラム路線に賭ける。L1 が絶対的に間違いだからではなく、L1 を正当化する条件が稀であり、ホストチェーン上でまだ出荷していないプロジェクトが多年がかりの L1 開発にコミットする資格をまだ得ていないからだ。Phase B は perp DEX プロジェクトの 99% が実際に取る路線を教える。

---

## §6.0  はじめに

Phase A はランタイム基礎を組み立てた。Phase B は HL プリミティブを組み立てる — そしてそれらはすべて他のプログラム、主に SPL Token と話す。注文配置のたびにトークンが動く。決済のたびにトークンが動く。出金のたびにトークンが動く。これらはどれも「本書のプログラム内で何かをする」ではなく、「SPL Token に何かを本書のプログラムの代わりにやらせる」だ。プログラム間のこの対話を **cross-program invocation (CPI)** と呼ぶ。所有者チェックの次に、本書のプログラムが行う最も結果の大きな振る舞いである。

本章では、必要な最も基本的な CPI を構築する: market のための SPL Token vault と、ユーザのトークンを vault に移す `Deposit` 命令。両者あわせて CPI 機構全体を実演する。

- **CreateVault** は 2 連続の CPI を行う — `System::create_account`（vault PDA が `invoke_signed` で署名）と `SPL Token InitializeAccount3`（誰の署名も要らない、ただの `invoke`）。
- **Deposit** は 1 つの CPI を行う — `SPL Token Transfer`。署名はユーザだが、PDA 経由ではなく、外側トランザクションからの**署名者特権の延長**で渡る。

終えるころには次のものを手にしている。

1. `invoke` と `invoke_signed` を並べて読み、両者が文字どおり「シード引数があるか」だけが異なる同じ呼び出しであると理解する。
2. ランタイムの署名者特権規則を歩く: 外側トランザクションの `AccountMeta` に現れた署名者が CPI の `AccountInfo` に再放出されると、被呼び出し側は署名者として扱う。
3. SPL Token 命令を 2 つ手で組み立て — `INITIALIZE_ACCOUNT_3`（tag 18）と `TRANSFER`（tag 3） — ワイヤ上に乗るバイトを正確に把握する。
4. なぜ vault 作成には `invoke_signed` を使い、入金には `invoke` を使うのかを理解する。両者は**入れ替え不可**だ — その選択は、その操作のために誰が署名しなければならないかが決める。

これが、プログラムを「自分のアカウントを所有する」段階から「チェーン上の他のすべてと話す」段階に進める章だ。Phase B のすべてはこの上に立つ。

---

## §6.1  `invoke` と `invoke_signed` は同じ呼び出し

`solana-cpi-2.2.1/src/lib.rs:137–139` を開こう。

```rust
pub fn invoke(instruction: &Instruction, account_infos: &[AccountInfo]) -> ProgramResult {
    invoke_signed(instruction, account_infos, &[])
}
```

これが `invoke` の本体すべて。`invoke_signed` を空のシードスライスで呼ぶだけだ。両者は内部で同じシステムコールで、唯一変わるのは PDA シードを供給するかどうか。

意味論上の区別は次のとおり。

- **`invoke_signed(ix, accounts, signers_seeds)`** — 「この命令を実行せよ。これらのシードから派生する PDA を追加の署名者として扱え」。第 3 章で `create_account` 時に market PDA のためにプログラムが署名できるようにするのに使った。
- **`invoke(ix, accounts)`** — 「この命令を実行せよ。本書から新たに署名者を追加しない。`accounts` 内にすでに存在する署名者（外側トランザクションに署名済み）は引き続き署名者として扱われる」。元のトランザクションの署名者がすでに必要な権限を持っているときに使う。

どちらを使うかは、CPI の操作のために**誰が署名しなければならないか**で完全に決まる。

- `create_account` の新アカウント? 自前で所有する PDA。本書しか署名できない。→ `invoke_signed`。
- SPL Token Transfer の transfer authority? **ユーザ**のウォレット鍵ペア。外側トランザクションに署名済み。→ `invoke`。
- `SPL Token MintTo` の mint authority? 場合による。本書のプログラムが mint authority なら（例: 自前で制御する PDA）、→ `invoke_signed`。ユーザが mint authority で外側 tx に署名しているなら、→ `invoke`。

第三の選択肢はない。すべての CPI は、外側トランザクションから署名者を継承するか、PDA シードで拡張するかのどちらかだ。ランタイムの仕事は、CPI が要求するすべての署名者が、どちらかの仕組みで説明できることを検証することだ。

**SDK が隠していること:** Anchor の `CpiContext::new(...)` と `CpiContext::new_with_signer(...)` は、それぞれ `invoke` と `invoke_signed` の直接ラッパだ。選択はあなたに委ねられる。Anchor が代わりに選ぶことはない。間違ったほうを選ぶと、ランタイムは署名エラーで CPI を失敗させる。

> **演習 §6.1.** CPI が PDA 署名者を必要としないときに、空でないシードスライスで `invoke_signed` を呼ぶと何が起きるか。（ヒント: エラーにはならない。ランタイムは、内側命令の必要署名者リストに現れない PDA 署名者を単に無視する。）

---

## §6.2  署名者特権の延長

`Deposit` を PDA 署名なしで成立させている規則は短い: **本書のプログラムの外側トランザクションの `AccountMeta` に署名者が現れ、CPI の `AccountInfo` でそれを再放出すると、被呼び出し側はそのアカウントを署名者として認識する**。

これがランタイムの「署名者特権の延長」だ。ユーザは外側トランザクションで**一度**署名する。`process_deposit` 内では、`user` アカウントの `AccountInfo` の `is_signer = true` だ（外側 tx で署名されているから）。本書が SPL Token Transfer 命令を `AccountMeta::new_readonly(*user_ai.key, true)`（signer = true）で組み立て、`invoke` の `account_infos` に `user_ai.clone()` を渡すと、ランタイムは確認する: 「このアカウントは自分のレベルで署名者とマークされているか? はい。CPI のアカウントリストでも署名者とマークされているか? はい。両者は一致するか?」 — 一致すれば、被呼び出しプログラム（SPL Token）は、`is_signer = true` の `AccountInfo` をユーザについて受け取る。

この規則が防いでいること: CPI を通じて非署名者を署名者に**昇格**させることはできない。ユーザが外側トランザクションに署名していなければ、CPI 内で `AccountMeta::new(*user.key, true)` を何度書こうと署名は生まれない。ランタイムは食い違いを見つけ、`MissingRequiredSignature` で CPI を拒否する。

この規則が許していること: 署名を再度頼まずに署名者を**伝播**できる。ユーザは一度署名する。その署名は、CPI 宣言でユーザを署名者として再放出する以降の連鎖上のすべてのプログラムに対して有効だ。これがあって、1 つのユーザ署名で SPL Token トランスファ、スワップ、プログラム呼び出しの任意の連鎖を 1 トランザクションで認可できる。

もう 1 つある。`invoke_signed` の `signers_seeds` パラメータから来る PDA 署名者は、CPI の間だけ外側署名者集合に**追加**される。ランタイムはシード + 呼び出し元プログラム ID をハッシュし、結果が署名対象アカウントと一致することを確認し、そのアカウントを CPI の署名者として扱う。これが PDA に署名する唯一の仕組みだ — 秘密鍵はないのだから。

両方の機構 — 外側 tx 伝播と PDA シード — は同じ `invoke_signed` 本体に住んでいる。ランタイムは内側命令の署名者を判定するときに 2 つの和集合を取る。

> **演習 §6.2.** `process_deposit` で、SPL Token Transfer 命令を `AccountMeta::new_readonly(*user_ai.key, false)`（signer = false）で組み立てると何が起きるか。トランスファは成功するか。なぜか。

---

## §6.3  CreateVault を歩く — 2 連続 CPI

CreateVault は、両方の CPI 形式が同じハンドラに現れる場所だ。`programs/openhl-core/src/lib.rs:680–720` から。

**PDA 派生**（668–678 行）が 2 つの pubkey を準備する: `[VAULT_SEED, market.key, mint.key]` の `vault_token_account` と、`[VAULT_AUTH_SEED, market.key]` の `vault_authority`。第 3 章と同じ find_program_address 機構。bump が返り、vault bump は下で使う。authority bump はまだ不要だ（出金を追加する後の章で、プログラムが vault authority として署名する必要が出てきたときに重要になる）。

**CPI 1 — System::create_account**、689–700 行。

```rust
let create_ix = system_instruction::create_account(
    payer_ai.key,
    vault_ai.key,
    rent,
    TOKEN_ACCOUNT_LEN as u64,
    &SPL_TOKEN_PROGRAM_ID,
);
invoke_signed(
    &create_ix,
    &[payer_ai.clone(), vault_ai.clone(), system_ai.clone()],
    &[&[
        VAULT_SEED,
        market_ai.key.as_ref(),
        mint_ai.key.as_ref(),
        &[vault_bump],
    ]],
)?;
```

ここが `invoke_signed` なのは、新アカウントが自前所有の PDA であり、System が新アカウントの署名を要求するからだ。シード + bump は `find_program_address` 呼び出しで使ったものと同じ。`create_account` の第 3 引数は新アカウントの**所有プログラム** — `SPL_TOKEN_PROGRAM_ID` を渡し、自前プログラムは渡さない。この瞬間から vault token account は SPL Token が所有することになり（Solana ランタイムレベルで）、データを書けるのは SPL Token だけになる。

AccountInfo 配列に注目: `[payer_ai, vault_ai, system_ai]`。System プログラムが `create_account` で必要とするのはこの 3 つだけ（ハンドラの他のアカウント — market、mint、vault_authority、token_program — は System が必要としないので渡さない）。

**CPI 2 — SPL Token InitializeAccount3**、707–720 行。

```rust
let mut init_data = Vec::with_capacity(1 + 32);
init_data.push(spl_token_ix::INITIALIZE_ACCOUNT_3);
init_data.extend_from_slice(vault_auth_ai.key.as_ref());
let init_ix = Instruction {
    program_id: SPL_TOKEN_PROGRAM_ID,
    accounts: vec![
        AccountMeta::new(*vault_ai.key, false),
        AccountMeta::new_readonly(*mint_ai.key, false),
    ],
    data: init_data,
};
invoke(&init_ix, &[vault_ai.clone(), mint_ai.clone(), token_ai.clone()])?;
```

これがプレーン `invoke` なのは、誰も署名する必要がないからだ。vault アカウントはすでに SPL Token 所有だ（CPI 1 でそうした）。InitializeAccount3 はただのデータ書き込み — 空のトークンアカウントに mint と owner フィールドを設定する。SPL Token に所有されているという事実自体が SPL Token によるデータ書き込みを認可するので、署名は要らない。

`invoke` に渡される *AccountInfo* 配列は `[vault_ai, mint_ai, token_ai]` だが、命令の *AccountMeta* に挙がっているのは `vault_ai` と `mint_ai` だけ — なぜ追加の `token_ai` が要るのか。ランタイムは、**呼び出し先プログラム**自身の AccountInfo も渡すことを要求する。これが「AccountInfo 再借用」規則だ: ランタイムが呼び出しを準備するために必要となる AccountInfo はすべて渡さなければならず、それには被呼び出しプログラム自身が含まれる。

CPI 2 つ、ハンドラ 1 つ、両方の署名形式。このパターンは Phase B 全体にわたって繰り返される。

> **演習 §6.3.** `invoke` 呼び出しから `token_ai.clone()` を削除せよ。ランタイムが返すエラーは何か。初期 Solana コードで最も頻繁に起きる CPI バグの 1 つで、最も Google しにくいエラーメッセージの 1 つだ。

---

## §6.4  Deposit を歩く — ユーザは外側で署名

Deposit はより単純なケースだ。`programs/openhl-core/src/lib.rs:771–800` から。

命令データ。

```rust
let mut transfer_data = Vec::with_capacity(1 + 8);
transfer_data.push(spl_token_ix::TRANSFER);
transfer_data.extend_from_slice(&amount.to_le_bytes());
```

合計 9 バイト: 1 バイトのタグ（Transfer は 3）と、リトルエンディアン 8 バイトの amount。これが SPL Token Transfer のワイヤフォーマット全体だ。

アカウント。

```rust
let transfer_ix = Instruction {
    program_id: SPL_TOKEN_PROGRAM_ID,
    accounts: vec![
        AccountMeta::new(*user_token_ai.key, false),
        AccountMeta::new(*vault_token_ai.key, false),
        AccountMeta::new_readonly(*user_ai.key, true),
    ],
    data: transfer_data,
};
```

3 アカウント: source トークンアカウント（書き込み可）、destination トークンアカウント（書き込み可）、authority（署名者、読み取り専用）。authority はユーザのウォレットで、署名者としてマークされている。ここではユーザのウォレットに**書き込まない** — トークンの移動はソーストークンアカウントの残高フィールドに反映され、ウォレットの lamports には反映されない — のでウォレット自身は read-only-but-signing になる。

CPI。

```rust
invoke(
    &transfer_ix,
    &[
        user_token_ai.clone(),
        vault_token_ai.clone(),
        user_ai.clone(),
        token_ai.clone(),
    ],
)?;
```

プレーン `invoke`。シードなし。ユーザは外側トランザクションに署名した。その署名が `user_ai` `AccountInfo` の `is_signer = true` フラグ経由で SPL Token に流れる。SPL Token は authority として受け入れる署名者を見て、トランスファが確定する。

これが「プログラム媒介ユーザトランスファ」の正典パターンだ: ユーザは外側トランザクションに署名して操作を認可する、プログラムは必要な CPI を組織化する、ユーザの署名者特権が流れていく。

> **演習 §6.4.** `user_token_account` と `vault_token_account` が異なる mint に属する Deposit トランザクションを組み立てよ。どのエラーが、どの層 — 本書のプログラムか SPL Token か — で表面化するか。

---

## §6.5  SPL Token 命令データを手で組み立てる

`spl-token` クレートは**意図的に**インポートしなかった。代わりに 2 命令分のバイトを手で組み立てた。`lib.rs` の 594–598 行。

```rust
mod spl_token_ix {
    pub const TRANSFER: u8 = 3;
    pub const INITIALIZE_ACCOUNT_3: u8 = 18;
}
```

これが必要な語彙のすべて。タグ値 2 つ。それ以外のすべて — フィールドエンコード、アカウント順 — は SPL Token プログラムのソース（あるいは `spl-token` クレートの `Instruction` enum、ただし意図的に依存していない）を読んで得る。

理由は 2 つ。

1. **教育。** 本章は CPI バイトについての章だ。正確なバイトを生成するビルダをインポートすれば、バイトを一度も見ずに章を終えられてしまう。一度手で組み立てればフォーマットを永続的に学べる — 将来出会うあらゆる SPL Token 命令で、ドキュメントを引かずにタグとデータが読める。
2. **バイナリサイズ。** `spl-token` クレートの最近のバージョンは、BPF コンパイル時に約 25 KB の依存関係（Token enum、エラー型、ヘルパビルダ）を引き連れてくる。2 命令しか必要としないプログラムにとっては純粋なオーバーヘッドだ。手で組み立てる方式はおそらく 200 バイト程度の追加で済む。

このトレードオフは大多数の本番プログラムは**しない** — 型安全性と保守ストーリーのために `spl-token` をインポートする。本書ではここでこのトレードを行う、章が要求するからだ。バイトを理解した後はトレードが逆向きになる: 25 KB を払い、型付きビルダにタグミスを捕捉させる。

この一般的な技法は、CPI する任意のプログラムに適用できる。

1. 被呼び出し側のソースで命令タグを見つける。
2. データフィールドを見つける（Borsh エンコード、または手作業詰め、プログラムによる）。
3. 被呼び出し側プロセッサのアカウントリストを見つける — 通常はバリアントのドキュメントコメントに `[WRITE, SIGNER]` のようなアカウント参照として書かれている。
4. `Instruction` 構造体を手で組み立てる。
5. `invoke` または `invoke_signed` する。

この 5 ステップのレシピは SPL Token、Address Lookup Table プログラム、Compute Budget プログラム、BPF Loader プログラム、誰かが作った任意の独自プログラムに通用する。機構は変わらない。

**Anchor が隠していること:** Anchor は SPL Token のための型付き CPI ラッパ（`anchor_spl::token::{Transfer, MintTo, ...}`）を持ち、バイトレイアウトを完全に隠す。動くが、本章があなたに見せたいまさにそれを覆い隠す。CPI を手で組み立て明示的に晒すネイティブプログラムは、`anchor_spl::token::transfer(ctx, amount)` を呼んでマクロを信用するプログラムより、はるかに優れたセキュリティ監査ストーリーを持つ。

---

## §6.6  まとめと自己検証

### まとめ図

```
                  invoke                            invoke_signed
                  ──────                            ─────────────
   実体:          invoke_signed(ix, accs, &[])      invoke_signed(ix, accs, seeds)
   PDA 署名:      なし                              あり（プログラムが派生 PDA に署名）
   外側署名者:    自動で伝播                        自動で伝播
   使い分け:      ユーザ / 外側 tx が操作に署名     自前所有の PDA が署名する必要


CPI ごとの特権解決:
    内側命令の各 AccountMeta（is_signer = true）について:
       同じ pubkey が**外側** AccountMeta で is_signer マーク?         ──► YES → 署名者 OK
       または、PDA としてこの pubkey を派生するシードを渡した?          ──► YES → 署名者 OK
       それ以外                                                          ──► CPI 拒否


openhl-core CPI マップ:

    CreateVault
       CPI 1: System::create_account
          invoke_signed、vault PDA シード
       CPI 2: SPL Token InitializeAccount3
          invoke（署名不要 — SPL Token が新アカウントを所有）

    Deposit
       CPI 1: SPL Token Transfer
          invoke（ユーザが外側 tx で署名、特権が流れる）
```

### 自分で検証する 3 項目

1. **`invoke` はシードなしの `invoke_signed`。** `solana-cpi-2.2.1/src/lib.rs:137` を開き、3 行の本体を読め。両者の違いは「PDA シードを渡したか」だけだと身体化せよ。それ以外のすべては同じシステムコール。
2. **被呼び出しプログラムの AccountInfo。** CreateVault の両 CPI で、`invoke[_signed]` に渡される `accounts` スライスは被呼び出しプログラムの AccountInfo を含む — CPI 1 では `system_ai`、CPI 2 では `token_ai`。これを忘れるのは初期 Solana で最も頻出のミスの 1 つで、ランタイムは紛らわしい `AccountNotFound` 風味のエラーを返す。[`lib.rs:691, 720`](../../programs/openhl-core/src/lib.rs#L691) を読んで確認せよ。
3. **ユーザは一度署名し、SPL Token がそれを使う。** バリデータに対して `deposit` を実行せよ。ユーザ鍵ペアが外側トランザクションに署名する。SPL Token は `is_signer = true`（ユーザについて）の `Transfer` 命令を受け取る。トランスファが確定する。どこにも 2 度目の署名はない。1 署名、伝播。

---

## 第 7 章への導線

これで他のプログラムと話せるようになった。しかし、Phase B で最も話す相手になるプログラムは**自分自身** — オンチェーン板だ。CLOB は market アカウントの中に bids と asks のスラブとして住み、すべての place/cancel 命令はその大部分を読み書きする。そのデータ構造が次に組み立てるものだ。

第 7 章ではオンチェーン CLOB 設計を歩く: critbit vs heap vs slab、`bytemuck` 経由のゼロコピーアカウントアクセス、メモリ局所性とアカウントサイズのトレードオフ、本番プログラムがほぼ常に slab を選ぶ理由。`Market` 構造体を固定容量の板を埋め込むよう拡張し、そこへ書き込む `place_order` 命令を追加し、板が満ちていくにつれて CU 包絡線が締まる様子を観察する。終わるころには market が初めての注文を受け取り、Phase A のコンピュートバジェット章を駆動した問い —「200K CU でマッチャをどう収めるか」— が初めて切迫したものになる。
