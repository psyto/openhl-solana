# 第4章 — コンピュートバジェットとヒープの規律

> 状態: ドラフト (v0.1)。
> 教材コード: [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（`process_bench`、355–402 行）、[`scripts/bench/src/main.rs`](../../scripts/bench/src/main.rs)。
> 検証対象バージョン: solana-program 2.3.0、solana-program-entrypoint 2.3.0、solana-compute-budget-interface 2.2.2。

---

## §4.0  はじめに

ここまでに書いた命令はすべて、ほぼ同じ程度のコンピュートしか消費していなかった — 数千ユニット程度。ランタイムのトランザクション単位上限 — 既定 200,000 コンピュートユニット (CU) — は視界にすら入ってこなかった。書いた量がほんの少しだったから、何を書いても許された。

その猶予は、何かに**比例した**処理を始めた瞬間に終わる。ループ内のハッシュ。板の線形走査。非自明な構造体の Borsh 復号。どれも入力に応じて CU が増えるプロファイルを持ち、そのプロファイルが他のどの制約より先に壊れる。

本章は数を勘定することを学ぶ場所だ。次の順に進める。

1. `solana-program::log` を開き、`sol_log_compute_units` を読む — 実行時に自身を計測するためにプログラムが持つ唯一の道具。
2. `solana-program-entrypoint` を開き、全プログラムのヒープを支える `BumpAllocator` を読む。`dealloc` が no-op である理由と、それが `Vec` や `Box` に何を意味するかを理解する。
3. 厳格な上限 — 既定ヒープ 32 KiB、既定 CU 200,000、絶対最大 CU 1.4M — と、それらが住む定数を確認する。
4. `solana-compute-budget-interface` を開き、`ComputeBudgetInstruction` enum を読む。これがトランザクション単位で CU 上限を引き上げる。
5. 新しい `process_bench` ハンドラを歩く。ヒープバッファを確保し、`sol_log_compute_units` で挟みながら sha256 を反復することで、フェーズごとの CU コストが読める形になっている。
6. 新しい `bench` クライアントを歩く。任意で `set_compute_unit_limit` を前置でき、プログラムログとランタイムの `units_consumed` 数値の両方を表示する。

終えるころには、出荷前に「この命令が既定バジェットに収まるか」を予測でき、収まらないときに何をリクエストすべきかが分かるようになる。

---

## §4.1  コンピュートユニットの正体

Solana の VM はサンドボックス化された BPF インタプリタだ。VM 内で実行される全命令には固定の CU コストが付く: 単純な ALU 演算は 1、ハッシュ syscall はもっと、別プログラムへの CPI はさらに多い。ランタイムはトランザクション単位のカウンタを持ち、上限（既定 200,000）から開始し、各操作の実行に応じて減算する。ゼロに達すると、トランザクションは `ComputationalBudgetExceeded` で中断される。

これは現実世界のミリ秒ではない。次のことが可能になるよう、ランタイムが定義する抽象的な経済単位だ。

1. **作業に対して公正に課金する** — 優先手数料は消費 CU に比例する。
2. **トランザクション実行時間に上限をつける**。ホスト時計を使わない（バリデータ間で非決定論になるから）。
3. **スケジューリングを予測可能にする** — トランザクションがブロックに収まるかをランタイムが事前判定できる。

トランザクション単位の既定 CU 上限は 200,000。トランザクションが要求できる最大は 1,400,000。両方ともネットワーク定数で、時とともに変わってきた。単一の Rust ファイルには存在しない（ランタイムの feature gate 内に住む）。最新値の確認はバリデータの CLI（`solana program-buffer-info` ほか）か公式ドキュメントが正しい場所だ。

コードに**ある**のは、プログラムが自身を計測する道具 — `sol_log_compute_units`、`solana-program-2.3.0/src/log.rs:92–101`。

```rust
/// Print the remaining compute units available to the program.
#[inline]
pub fn sol_log_compute_units() {
    #[cfg(target_os = "solana")]
    unsafe {
        crate::syscalls::sol_log_compute_units_();
    }
    #[cfg(not(target_os = "solana"))]
    crate::program_stubs::sol_log_compute_units();
}
```

syscall が 1 つ。呼ばれた瞬間の**残り** CU を出力する。連続する 2 つの読み値を引き算すれば、その間に行った作業のコストが出る。道具はそれだけだ。

**SDK が隠していること:** Anchor はこれらの呼び出しを自動挿入しない。Anchor プログラムで CU 計測がしたければ、`solana_program::log::sol_log_compute_units();` を自分で書く — まさに本書と同じだ。

> **演習 §4.1.** noop 命令の中で `sol_log_compute_units` を 2 行連続で呼び（1 行間隔で）、1 回の呼び出し自体の CU コストを算出せよ。最初の読み値から 2 つ目を引いた差が、この呼び出し自身のコストになる。ほとんどのプログラムは会計上ゼロとして扱うが、実際には十数 CU だ。

---

## §4.2  ヒープ — 解放しない bump アロケータ

Solana プログラムは固定サイズのヒープアリーナで動く。既定サイズは `solana-program-entrypoint-2.3.0/src/lib.rs:40–42`。

```rust
pub const HEAP_START_ADDRESS: u64 = 0x300000000;
// ...
pub const HEAP_LENGTH: usize = 32 * 1024;
```

32 キビバイト。これが、命令 1 つの実行中にプログラムが確保するすべての `Vec`、`Box`、`String`、`HashMap` に使えるヒープ全体だ。使い切ると、グローバルアロケータは null ポインタを返し、Rust のアロケーション失敗ハンドラがプログラムを中断する。

ヒープを支えるアロケータは `lib.rs:291–302`。

```rust
pub struct BumpAllocator {
    pub start: usize,
    pub len: usize,
}
```

そして `GlobalAlloc` 実装は `lib.rs:342–364`。

```rust
unsafe impl std::alloc::GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pos_ptr = self.start as *mut usize;
        let mut pos = *pos_ptr;
        if pos == 0 {
            pos = self.start + self.len;
        }
        pos = pos.saturating_sub(layout.size());
        pos &= !(layout.align().wrapping_sub(1));
        if pos < self.start + size_of::<*mut u8>() {
            return null_mut();
        }
        *pos_ptr = pos;
        pos as *mut u8
    }
    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {
        // I'm a bump allocator, I don't free
    }
}
```

吸収すべき点は 2 つ。

1 つ目、**`dealloc` は no-op**。`Vec` を drop しても、`Box` を drop しても、関数から return しても、確保した領域は命令の残り時間ずっと確保されたままだ。すべての確保が、プログラムが終わるまで永続する。これは意図的なトレードだ — 本物のフリーリスト型アロケータは維持に CU を食うし、命令 1 つの寿命は短いので断片化はヒープサイズで上限を取れる。

2 つ目、**bump ポインタは末尾から下方向に伸びる**（`pos = pos.saturating_sub(layout.size())` を見よ）。`pos` がヒープ基底より下に落ちると、`alloc` は null を返し、プログラムはパニックする。本書の `Bench` ではこれを意図的に発火させる — `--heap-bytes 65536`（ヒープサイズの 2 倍）を渡せば OOM になる。

**SDK が隠していること:** Anchor は確保を**推奨**したことは一度もないが、`Vec` バックドアカウント（`Vec<Pubkey>`、`BTreeMap` 等）は内部でこのヒープに依存している。10,000 エントリのベクタをアカウントからデシリアライズすると、軽快に ~80 KiB のヒープを請求してプログラムがクラッシュする — 「ヒープは 32 KiB で何も解放しない」を思い出すまで、原因がわからない。

> **演習 §4.2.** `process_bench` に 2 つ目の `vec![0u8; heap_bytes]` 確保を、1 つ目の直後に追加せよ。`--heap-bytes 8192` ならプログラムは成功する（8 KiB + 8 KiB ≈ 16 KiB、32 KiB 以内）。`--heap-bytes 16384` だと OOM になる。両方の結末を確認せよ。

---

## §4.3  上限を上げる — `ComputeBudgetInstruction`

短い命令なら既定の CU 上限で十分だ。それ以上 — Borsh 復号、CLOB マッチ、多段 CPI チェーン — のためには、追加を明示的に要求しなければならない。仕組みは、ユーザプログラムが走る前にランタイムが処理する、トランザクションレベルの特別な命令だ。

`solana-compute-budget-interface-2.2.2/src/lib.rs:24–38` から。

```rust
pub enum ComputeBudgetInstruction {
    Unused,
    /// Request a specific transaction-wide program heap region size in bytes.
    /// The value requested must be a multiple of 1024.
    RequestHeapFrame(u32),
    /// Set a specific compute unit limit that the transaction is allowed to consume.
    SetComputeUnitLimit(u32),
    /// Set a compute unit price in "micro-lamports" to pay a higher transaction
    /// fee for higher transaction prioritization.
    SetComputeUnitPrice(u64),
    /// Set a specific transaction-wide account data size limit, in bytes, is allowed to load.
    SetLoadedAccountsDataSizeLimit(u32),
}
```

レバーは 4 つ、それぞれにコンストラクタが `lib.rs:55–67` にある。日常的に使うのは次の 2 つ。

- **`set_compute_unit_limit(units)`** — トランザクション全体の CU 上限を引き上げる。1,400,000 までの任意の値を渡せる。
- **`request_heap_frame(bytes)`** — プログラムあたりのヒープサイズを引き上げる。1024 の倍数でなければならない。32 KiB 以上のヒープが本当に必要なときに使う。

本書の `bench` クライアントは `scripts/bench/src/main.rs:101–102` で `set_compute_unit_limit` を使う。

```rust
if let Some(limit) = cli.cu_limit {
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(limit));
}
```

compute-budget 命令はトランザクション内の位置に関わらずランタイムが処理するが、読みやすさのため慣例で最初に置く。アカウントリストはない — ランタイムの実行前フェーズがデータとして解釈するだけだ。

覚えておきたい性質が 3 つ。

1. **同種は 1 トランザクションに 1 つ。** 同じ tx 内の 2 つ目の `SetComputeUnitLimit` は `DuplicateInstruction` で拒否される。
2. **部分返金はない。** 1M CU を要求してプログラムが 50K しか使わなくても、優先手数料の計算では 1M 上限が基準になる（通常のトランザクション手数料には影響しない）。
3. **それ自体が CU を消費する。** compute-budget 命令の処理に約 150 CU かかり、トランザクション合計に含まれる。

**SDK が隠していること:** Anchor は compute-budget 命令を自動で前置しない。クライアント側で、Anchor が生成した命令の前に自分で追加する。これを忘れて、ハンドラが 200K CU を超えた瞬間に「transaction simulation failed」という謎エラーが出る Anchor ユーザは多い。

> **演習 §4.3.** `--cu-limit` なしで `bench --rounds 200 --heap-bytes 256` を実行せよ。`units_consumed` の値を控える。次に `--cu-limit 50000` を追加する。プログラムは成功するか失敗するか。なぜか。（ヒント: 1 回目の `units_consumed` と設定した上限を比べよ。）

---

## §4.4  `process_bench` を歩く

ハンドラは小さい — `programs/openhl-core/src/lib.rs:355–402` の約 50 行。構造は、`sol_log_compute_units` で挟まれた 3 フェーズだ。

**入口。** 8 バイトのペイロードを復号し（`rounds: u32 LE`、`heap_bytes: u32 LE`）、開始 CU を記録する。

```rust
msg!("bench: start (rounds={}, heap_bytes={})", rounds, heap_bytes);
sol_log_compute_units();
```

**フェーズ A — ヒープ。** バッファを確保し、もう一度記録する。

```rust
let mut buf = vec![0u8; heap_bytes as usize];
msg!("bench: after heap alloc ({} bytes)", buf.len());
sol_log_compute_units();
```

`vec!` マクロが bump アロケータを呼ぶ。最初のログ読み値からこの読み値を引いた差が、`heap_bytes` バイト確保の**コスト**だ。驚くほど小さい — bump アロケータはポインタ減算 1 つだけだから — が、syscall スタブのオーバーヘッドに比例する形になり、バイト数には比例しない。

**フェーズ B — ハッシュループ。** sha256 を `rounds` 回反復し、毎回ダイジェストをバッファに戻す。

```rust
for i in 0..rounds {
    let digest = sha256(&buf);
    let bytes = digest.to_bytes();
    let copy_len = bytes.len().min(buf.len());
    buf[..copy_len].copy_from_slice(&bytes[..copy_len]);
    if !buf.is_empty() {
        buf[0] ^= i as u8;
    }
}
```

ここが実際に CU を燃やす作業だ。`sha256` は BPF 上では syscall（`sol_sha256_`）で、コストは入力長に依存する。フェーズ A の読み値からフェーズ B の読み値を引いた差が、1 ラウンドあたりの CU コスト — おおよそ `(sha256 syscall 基本コスト) + (バイトあたりコスト × heap_bytes)` だ。

`lib.rs:392` のカウンタとの XOR は、オプティマイザに正直でいてもらうために存在する。これがないと、毎回同じバイトをハッシュすることになり、十分に攻撃的なオプティマイザはループを潰してしまう可能性がある。`i` をバッファに混ぜることで、各反復の入力が本当に異なるものになる。

最後の `_` 読み値は、関数 return 時に `sol_log_compute_units` が自動で残す（ランタイム自身が「consumed N of M compute units」というログ行をプログラム終了後に出力する形で）。

> **演習 §4.4.** `bench --rounds 0 --heap-bytes 0` と `bench --rounds 0 --heap-bytes 1024` を実行せよ。「after heap alloc」の CU 読み値を引き算する。その差が、bump ヒープから 1024 バイトを確保するコストだ。予想より大きいか小さいか。なぜか。

---

## §4.5  bench 出力を読む

典型的な実行例。

```
bench --rounds 50 --heap-bytes 1024

simulation:
  units_consumed: 87412
  err:            (none)

program logs:
  Program <openhl-core ID> invoke [1]
  Program log: bench: start (rounds=50, heap_bytes=1024)
  Program consumption: 199772 units remaining
  Program log: bench: after heap alloc (1024 bytes)
  Program consumption: 199639 units remaining
  Program log: bench: after 50 hash rounds
  Program consumption: 112433 units remaining
  Program <openhl-core ID> consumed 87412 of 200000 compute units
  Program <openhl-core ID> success

on-chain signature: ...
```

注目したい数値は 3 つ。入口直後の残り CU（199,772）が、プログラム起動の固定コストを示す — 命令データ復号、ディスパッチャ、ログ — このビルドでは約 230 CU。フェーズ A が消費したのは (199,772 − 199,639) = 133 CU、1 KiB の確保に対して。フェーズ B は (199,639 − 112,433) = 87,206 CU を消費、1 KiB 入力で 50 ラウンドの sha256 — 1 ラウンドあたり約 1,744 CU。

ここから外挿できる。100 ラウンド: 約 175K CU。120 ラウンド: 約 209K CU — 既定 200K 上限を超える。`--cu-limit` なしで実行すれば `ComputationalBudgetExceeded` 付きの `ProgramFailedToComplete` が出る。`--cu-limit 400000` を付ければまた成功する。

これがこの章の核心だ。1 度測り、予測し、その上で作業をバジェットに収めるか、明示的に上限を引き上げるかを決める。推測は、まだ負荷下で出荷していないプロジェクトのやり方だ。

**SDK が隠していること:** Anchor のログにも同じ「consumed N of M compute units」行が出る。プログラムからではなくランタイムから出ているからだ。しかし Anchor は `units_consumed` を型付きフィールドとしてどこにも公開しない — 本書と同じくバリデータログから読み取る。

> **演習 §4.5.** `--cu-limit` を、前回の `units_consumed` をわずかに下回る値に設定せよ。トランザクションは `ComputationalBudgetExceeded` で失敗するはずだ。次にわずかに上回る値を試す。成功するはずだ。境界は厳密で、それゆえに CU は計画ツールとして使える。

---

## §4.6  Anchor が CU について隠していること

Anchor は CU 計測を一切挿入しない。バジェットを自動で引き上げない。ハンドラが長すぎることをコンパイル時に警告しない。CU は Anchor が完全にあなたに任せている数少ない要素のひとつだ — 汎用解が存在せず、どんな既定値も間違いになるからだ。

Anchor が**やる**のは、型付きアカウントごとに概ね 2,000〜5,000 CU のオーバーヘッドを加えること — 自動で行われるデシリアライズ + ディスクリミネータチェック + 所有者チェックのために。`Account<'info, T>` を 5 つパラメータに取るハンドラは、自分のコードが走り始める前に、型付きアカウントラッパだけで 15,000〜25,000 CU を払うことになる。本書では同等の手作業チェックに `process_initialize` と `process_create_market` でおよそ 600〜1,000 CU しか払わない。必要なものだけを手で組み立てているからだ。

これがネイティブプログラムの CU 上の論拠そのものだ: デシリアライザ、所有者チェック、借用を自分で書くなら、コストは自分で制御できる。Anchor がやるなら、コストは Anchor の既定値のコストになる。market 作成のように 1 回しか呼ばれないハンドラなら、差は無視できる。約定ごとに呼ばれるループ内ハンドラなら、その差がビジネス全体だ。

---

## §4.7  まとめと自己検証

### まとめ図

```
トランザクション単位の上限
┌────────────────────────────────────────────────────────────┐
│                                                            │
│  CU         既定 200,000     最大 1,400,000                 │
│  ヒープ     既定 32 KiB      最大 256 KiB（RHF 経由）        │
│  データ量   既定 約 64 MB    tx ごとに調整可                 │
│                                                            │
└────────────────────────────────────────────────────────────┘

1 tx 内:
  ┌─────────────────────────┐     ┌──────────────────────┐
  │ ComputeBudgetInstruction│ ──► │ ランタイムが tx 内の  │
  │ (set_compute_unit_limit │     │ 全ユーザプログラムに  │
  │  / request_heap_frame)  │     │ 上限を適用する        │
  └─────────────────────────┘     └──────────────────────┘
                                            │
                                            ▼
  ┌─────────────────────────┐     ┌──────────────────────┐
  │ openhl-core::Bench      │ ──► │ 各フェーズで          │
  │   - ペイロード復号       │     │ sol_log_compute_units│
  │   - vec![0u8; n] ヒープ │     │ 成功か CU=0 まで実行  │
  │   - sha256 ループ       │     │                      │
  └─────────────────────────┘     └──────────────────────┘
```

### 自分で検証する 3 項目

1. **既定上限は本物。** `--cu-limit` なしで `bench --rounds 150 --heap-bytes 1024` を実行せよ。`ComputationalBudgetExceeded` で失敗するはずだ。シミュレーションの `units_consumed` は 200,000 より大きな数値を示す可能性があるが、ランタイムは上限を超えた時点で計測を打ち切るので、数値が頭打ちになる場合がある。
2. **ヒープ上限は本物。** `bench --rounds 0 --heap-bytes 65536` を実行せよ。[`solana-program-entrypoint-2.3.0/src/lib.rs:342`](#) の bump アロケータが null を返し、Rust の alloc-error ハンドラがプログラムを中断する。見えるエラーは `ComputationalBudgetExceeded` ではなく、メモリ中断になる。
3. **compute-budget 命令は同じ tx 内に必須。** `bench` を編集し、`ComputeBudgetInstruction` を bench 命令とは**別の**トランザクションで送るようにせよ。次の bench tx は依然として既定 200,000 しか受け取らない。compute-budget 命令の効果は、それ自身のトランザクション内に閉じる — 永続しない。

---

## 第 5 章への導線

自分のコードが何を消費するかを計測し、必要なバジェットを要求できるようになった。しかし CU はスループット物語の半分でしかない。もう半分は**並列性**だ — 同じアカウントに対していくつのトランザクションを同時実行できるか。Solana の看板機能 — 1 秒あたり数万トランザクションを処理できる理由 — は Sealevel スケジューラで、アカウントアクセスセットが衝突しない限り並列にトランザクションを走らせる。

第 5 章では、Sealevel が並列実行可能性を判定する read/write セットモデルを歩く。全 market を単一の「グローバルレジストリ」アカウントの背後に置くとプログラム全体がシングルスレッドになる理由、本書の `CreateMarket` PDA スキームが任意数の market を並行作成できる理由を見る。意図的に衝突する `Stats` アカウントを `openhl-core` に追加してスケジューラから見た直列化の姿を実演し、そして — 本物のプログラムを出荷する前に必ずやる — その `Stats` をリファクタリングして外す。
