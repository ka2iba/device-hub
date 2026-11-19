# Logitech G600 対応 — Codex 向け作業指示書

作成: 2026-11-19 (rev3: 明示保存・退避書き込み方針) / 対象: `crates/devhub-core`, `crates/devhub-hid`, `crates/device-hub` (最小限)

この指示書は 2 フェーズに分かれる。**Phase 0 の実機結果が出るまで Phase 1 に着手しない**
(registry の usage_page などが Phase 0 の結果で決まるため)。

---

## 0. 実行環境 (WSL / Linux) — 構築済み

Codex は WSL 上で作業し、ビルド・テストもそこで行う。実機 (HID) は WSL から見えないので、
実機手順は Windows 側でユーザーが行う。

- Rust: rustup + 1.95.0 (`rust-toolchain.toml` で自動選択)。clippy / rustfmt 入り。
  - `~/.cargo/env` は `.bashrc` / `.profile` から読まれる。
- ビルド用の dev ライブラリ (libudev, fontconfig, freetype, xkbcommon, wayland, x11, vulkan, openssl 等):
  - sudo なしで `~/.local/sysroot` に展開してある。
  - 参照先はユーザー設定 `~/.cargo/config.toml` の `[env]` (`PKG_CONFIG_PATH` / `C_INCLUDE_PATH` /
    `LIBRARY_PATH`) で渡している。
- GPUI の Linux 用パッチ (AGENTS.md の「wgpu 側 2 行パッチ」):
  - registry に展開済みの `gpui-pre-wgpu-0.3.12` へ直接当ててある。
  - `gpui-pre-wgpu` のコンパイルエラーが再発したら `~/.local/share/devhub-linux/patch-gpui-wgpu.sh` を実行する。

**禁止**:

- リポジトリに `.cargo/config.toml` を作らない。
- `Cargo.lock` の変更をコミットしない。依存追加は不要の想定。もし `Cargo.lock` が変わったら理由を報告する。

基準値 (2026-11-19 時点):

- `cargo test --workspace` は全件成功。
- `cargo clippy -p devhub-core -p devhub-hid` は警告 0。
- `cargo clippy -p device-hub` は既存の警告あり (devhub-polling 3 件、device-hub lib 4 件)。

## 1. 目的

Logitech G600 (046D:C24A) のオンボードプロファイル 3 本を Device HUB から読み書きできるようにする。
UI は caps で自動的に出るので、作業はドライバ + registry が中心。契約変更は G-Shift 用の
**追加のみ**。

## 2. 背景 / 参照

- 参考実装 (Python): https://github.com/ecerulm/python-hidapi-logitech-g600
  - **LICENSE ファイルが無い** → コードやサンプルのバイト列はコピーしない。レイアウトの事実だけを使う。
- 一次情報 (MIT): libratbag `src/driver-logitech-g600.c`
  - 参考実装もこれを下敷きにしている。2 つの記述が食い違う箇所は libratbag を正とする (§3.3)。
- プロトコルは HID++ ではなく、**番号付き feature report だけ**で完結する。
  - 既存 `transport::Channel` の feature API は report ID 0 の 64B 固定なので、そのままでは使えない。

## 3. プロトコル仕様 (確定事項と要実機確認事項)

### 3.1 レポート一覧

| report ID | 長さ (ID 込み) | 方向 | 内容 |
|---|---|---|---|
| 0xF0 | 4 | GET | 現在状態: `b1` の bit1-2 = 現在の解像度 index (0-3)、bit4-7 = アクティブプロファイル (0-2) |
| 0xF0 | 4 | SET | `[0xF0, 0x80 \| (p<<4), 0, 0]` = プロファイル p をアクティブに |
| 0xF0 | 4 | SET | `[0xF0, 0x40 \| (r<<1), 0, 0]` = 現在の解像度を r に (**非永続と推定**、Phase 0 で確認) |
| 0xF3/F4/F5 | 154 | GET/SET | プロファイル 0/1/2 の全内容 (§3.2) |

- GET の `b1` に SET 時の bit7 (0x80) は立たない前提 (libratbag は bit4-7 をそのまま profile として読む)。
- libratbag では、プロファイル切替の直後と、アクティブプロファイルへの書き込みの後に、
  現在解像度を再設定している。本実装でも同じ順序を踏襲する (§5.4 の 3)。

### 3.2 プロファイル (154B) のレイアウト

| offset | 長さ | 内容 |
|---|---|---|
| 0 | 1 | report ID |
| 1-3 | 3 | LED R,G,B |
| 4 | 1 | LED エフェクト 0=固定 1=ブリージング 2=カラーサイクル |
| 5 | 1 | LED 周期 (秒, 0-15。大きいほど遅い) |
| 6-10 | 5 | 不明 → **読んだ値を保持** |
| 11 | 1 | ポーリング: Hz = 1000/(v+1)。0=1000, 1=500, 3=250, 7=125 |
| 12 | 1 | DPI シフト値 (×50 dpi、0=無効) |
| 13 | 1 | 既定 DPI ステージ (**1 始まり**、§3.3) |
| 14-17 | 4 | DPI ステージ 1-4 (×50 dpi、範囲 0x04-0xA4 = 200-8200、**0=無効**) |
| 18-30 | 13 | 不明 → **読んだ値を保持** |
| 31-90 | 60 | 通常ボタン G1-G20 (各 3B: code, modifier, key) |
| 91-93 | 3 | G-Shift 中の LED 色 R,G,B |
| 94-153 | 60 | G-Shift 中のボタン G1-G20 |

ボタンの 3 バイト `(code, modifier, key)`:

| code | 意味 | ButtonAction |
|---|---|---|
| 0x00 + mod=0,key=0 | 無割当 | `Disabled` |
| 0x00 + その他 | キー (modifier は契約の `mods` と同じビット並び、key は HID Keyboard page usage) | `Key(KeyStroke{mods, usage: key})` |
| 0x01-0x05 | マウスボタン 1-5 | `Mouse(Left/Right/Middle/Back/Forward)` |
| 0x11 / 0x12 / 0x13 | DPI + / DPI − / DPI 循環 | `Dpi(Up/Down/Cycle)` |
| 0x14 | プロファイル順送り | `ProfileCycle` |
| 0x15 | DPI シフト (値は offset 12) | `Dpi(Shift(offset12×50))`。offset 12 が 0 なら `Shift(200)` として読む |
| 0x17 | G-Shift | **`GShift` (今回追加)** |
| その他 | 不明 | `Unsupported{ty: code, params: vec![modifier, key]}` (往復保存) |

### 3.3 要実機確認事項 (Phase 0 で確定させる)

1. **Windows でどの HID コレクションが 0xF0/0xF3-F5 を持つか** (if 番号・usage_page・usage)。
   - G600 はマウス IF とキーボード IF を持つ。
   - キーボード系のコレクションは OS が排他で持つが、hidapi は access=0 で開き直すので
     feature report は通るはず。
2. offset 13 の基数。
   - libratbag は「1-4」、Python 版の writer は「0-3」で書いている。
   - libratbag のコメントと Python 版の reader 側サンプルデータ (DPI1 のみ有効で default=1) から
     **1 始まりと推定**。実機の読み出し値で確認する。
3. `get_feature_report` に必要なバッファ長 (Windows は FeatureReportByteLength 以上を要求する)。
   256B バッファで受け、返った長さを記録する。
4. 書き込みが 1 回で確実に反映されるか。
   - Python 版 README は「数回やらないと反映されないことがある」と書いている (macOS)。
   - 同一内容の書き戻し→読み戻し比較で確認する。
5. **4B の短い feature report (0xF0 SET) が Windows で通るか**。
   - コレクションの FeatureReportByteLength は 154 以上のはず。hidapi が 0 埋めして送るかを確認する。
6. **開いた直後に読めるか**。Python 版は open 後に 2 秒待っている (macOS)。
   待ちなしで読めるなら、ドライバに待ちは入れない。

### 3.4 Phase 0 の実機結果 (2026-11-19、Windows 11、g600probe)

| §3.3 | 結果 |
|---|---|
| 1 | **確定**: `if=1 usage_page=0xFF80 usage=0x0080` (MI_01 Col02)。registry は `Some(0xFF80), Some(0x80)`。他のコレクション (マウス 0001/02、キーボード 0001/06、VHF 0059/01) は feature report を持たない |
| 2 | **確定: 1 始まり**。F3 = 段 1 のみ有効で値 1、F5 = 値 2 → 段 2。0xF0 の現在段とも矛盾なし |
| 3 | **確定**: 256B バッファで GET すると 4B / 154B が返る |
| 4 | **NG — 保存内容が破損** (下記「書き込み事故」) |
| 5 | 4B の SET は `Ok` が返る。効果は未確定 (0x19→0x11→0x10 と下位ビットが変化) |
| 6 | **確定**: 開いた直後に読める → ドライバに待ちは入れない |

追加の知見:

- **0xF0 GET の実値は `F0 19 00 00`**。
  - 0x19 = profile 1 / res 0。bit0 と bit3 も立っている。
  - profile は `(b1 >> 4) & 0x0F`、res は `(b1 >> 1) & 3` で取り出し、他のビットは無視する。
- **未知のボタンコード `0x16`** が F5 の G16 にある。`Unsupported` として往復保存する (表に追加しない)。
- 不明バイト: offset 24 は 3 プロファイルとも 0x02、他の 6-10 / 18-30 は 0。RMW で保持する。
- G-Shift 側が全 0 (= 全ボタン無割当) のプロファイルがある (F5)。正常な状態として読めること。
- コレクション `[2]` (usage page 0x59 = LampArray) は Windows の Dynamic Lighting 用 VHF。
  Windows の設定で Dynamic Lighting が有効だと LED 色を OS が上書きする可能性がある → docs のリスクに書く。

### 3.5 書き込み事故 (2026-11-19) — 対策は §3.8 の手順と §5.0 の方針

`--roundtrip 1` で、アクティブだった F4 を読んだ 154B と同じ内容で書き戻した (hidapi `send_feature_report`、
Windows、コレクション `[1]`)。

1. 直後の読み戻しは**完全一致**、`Ok(())`。
2. しかし直後からマウスが異常になった (感度極小・クリック無効)。G Hub 起動中だけ正常 (PC 側の上書き)。
3. 後で読み直した F4 は、**offset 4 に F1 (offset 3 の複製) が挿入され、以降が 1 バイトずつ後ろへずれ、
   末尾 1 バイトが落ちていた**。
   - 結果: G1 = `00 01 00` (Left Ctrl のみ)、DPI 段 1 = 50dpi、既定段 = 0。症状と一致する。
4. 復旧: `examples/g600select.rs` で 0xF0 `[F0 80 00 00]` (プロファイル 0 を選択) を送り、無事な F3 に切替えて回復。
   - 元の F4 は `.private-analysis/g600/F4_original_2026-11-19.txt` (Git 管理外) に保存。

ここから言えること:

- **書いた直後の GET は、保存内容ではなく送った内容を返すことがある** → §5.4 の「読み戻し比較」では破損を検出できない。
- 挿入位置 (offset 4) が 8B / 64B 境界と一致せず、原因 (Windows の SET_REPORT 経路か、ファームの書き込みか) は未特定。
  - Python 版 README の「数回やらないと反映されない」も、同種の不安定さの可能性。
- 0xF0 の解釈も再検討が要る。前回「段 0 の SET で感度が落ちた」と推測したが、感度低下は F4 破損 (50dpi) で説明できる。
- `g600probe` から書き込みモードは削除済み (読み取り専用)。

### 3.6 読み取り調査 (2026-11-19、事故後・F3 アクティブ)

- **Windows のレポート記述子** (コレクション `[1]`):
  - Feature: F0=3B, F1=7B, F2=4B, F3/F4/F5=153B, F6=7B (いずれも +1 ID)。
  - Input: 0x80=5B, F6=7B, F7=31B。
  - macOS で取られた記述子 (Apple Developer Forums 添付) と一致する。**送った 154B は宣言どおり → 長さ起因ではない**。
- **破損は本体側の保存処理で起きたと推定**: Windows は feature report をキャッシュしないので、
  書き込み直後の GET 一致は「本体が正しい 154B を受け取った」ことを意味する。
  それでも保存・反映された内容はずれていた。
- **0xF1** = `F6 00 FF 02 04 00 00`。末尾がアクティブな F3 の LED (サイクル・周期 4) と一致 → 現在の LED 状態と推定。
- **0xF2** = `00 10 1C 02` → 後のダンプでは `00 10 18 02`。値が変わるので FW バージョンではない (用途不明)。
- 0xF1 は後のダンプで `00 58 FF 02 04 00 00`。色の部分だけが変わり、エフェクトと周期は同じ → 「現在の LED 状態」説を補強。
- **0xF6** の GET は本体が STALL (エラー 0x1F)。
- **0xF0 の bit3 はプロファイル正常フラグの可能性**: 正常な F4 で 0x19 (bit3=1) → 破損直後 0x11 (bit3=0)
  → 正常な F3 で 0x08 (bit3=1)。3 点のみなので仮説。
- 次の実験 (E1、要了承): F3 アクティブのまま F4 にだけ元の内容を書く。
  直後の GET と挿し直し後の GET を比べ、再現性と bit3 の挙動を見る。

### 3.7 実験 E1 の結果 (2026-11-19、`examples/g600e1.rs`) — 成功

条件: F3 アクティブ、F4 (非アクティブ) に元の 154B を 1 回書く。open 後 2 秒待ち、書き込み中はマウスに触らない。

| タイミング | 0xF0 b1 | bit3 | F4 |
|---|---|---|---|
| 書く前 | 0x08 | 1 | 破損 (offset 4 に F1 挿入) |
| 書いた直後 | 0x00 | 0 | 元データと一致 (送った内容の可能性あり) |
| USB 挿し直し後 | 0x08 | 1 | **元データと一致 (保存内容)** → F4 は復元済み |

- **bit3 は正常フラグではない**。正常な書き込みでも 0 になり、再読み込みで 1 に戻る。
  「書き込みあり (未反映)」系のフラグと推定する (§3.6 の仮説は棄却)。
- 失敗 (§3.5) との違い:
  1. 書き込み先がアクティブか
  2. open 後の待ち (0 秒 / 2 秒)
  3. マウス操作の有無
  4. 書く直前の GET の有無
  - どれが効いたかは未特定。1 回の成功では安定性は判断できない。
- 次の候補 (いずれも非アクティブの F4 のみ対象):
  - E2: 同条件で繰り返して失敗率を測る。
  - E3: 待ちなし・操作ありで書き、条件を絞る。
  - E4: 書いた後に現在のプロファイルを再選択して、bit3 が 1 に戻るか (= 挿し直しなしで再読み込みできるか) を見る。

### 3.8 実験 E4 の結果 (2026-11-19) と安全な書き込み手順 (案)

E4 (F4 に同一内容を 1 回書き、観察): 挿し直し後も F4 は一致。

- **bit3 = 保存処理中フラグと推定** (0 = 処理中、1 = 待機)。
  SET の 500ms 後は 0、約 600ms 後には 1 に戻った (再選択は不要)。
- 書いた直後でも F3 / F5 はそれぞれ自分の内容を返す (単一の受信バッファを返すだけの作りではない)。
  ただし §3.5 で読み戻し一致にもかかわらず保存内容がずれたので、読み戻しは補助的な確認にとどまる。
- 現在プロファイルの再選択では観察できる変化なし (挿し直しなしの再読み込み手段は未確立)。
- §3.5 の事故は、**bit3=0 (処理中) の間に 0xF0 SET を送った**、かつ **open 直後に待ちなしで書いた**状態で起きた。
  E1 / E4 はいずれも該当しない。処理中の割り込みが最有力候補 (未証明)。

安全な書き込み手順 (Phase 1 の §5.4-3 をこれで置き換える予定):

1. open 後 2 秒待つ (ドライバ生成時に 1 回)。
2. 書く前に 0xF0 を読み、bit3=1 になるまで待つ (100ms 間隔、最大 3 秒。超えたら `Protocol` エラーで書かない)。
3. 154B を 1 回 SET する。
4. 300ms 待ち、0xF0 だけを 100ms 間隔で読んで bit3=1 を待つ (最大 3 秒)。**この間は他の SET を一切送らない**。
5. GET で照合する。不一致なら 1 回だけ再書き込み → それでも不一致なら `Protocol` エラーにして以降の書き込みを止める。

未決事項: 書き込みタイミング (明示保存 / 自動まとめ書き)、使用中プロファイルへの書き込み検証 (E5) を行うか。

### 3.9 アプリからの保存テストと DPI の反映 (2026-11-19)

アプリの「本体に保存」(退避 → 書き込み → 戻す) で、ボタン・ポーリング・LED は即時に反映された。**DPI だけは挿し直すまで反映されない**。

- 保存直後の F4: DPI 段 1 = `A4` (8200)。保存自体はできている。
- **0xF2 の 4 バイト目 = 今効いている DPI (×50)**。保存直後は `18` (1200)、挿し直し後は `A4` (8200) で、体感と一致する。
  - 読み取りだけで「今効いている DPI」を確認できる (以前の「FW バージョン候補」は誤り)。
- **0xF1 は固定エフェクトでも LED 設定を映す**。F4 = 固定・周期 1 のとき 0xF1 = `00 00 00 00 01` (色 + エフェクト + 周期)。保存後の反映チェックの前提が補強された。
- プロファイルの再選択では DPI は反映されない。
  - libratbag は書き込み・切替の後に、必ず解像度 SET (`0x40|r<<1`) を送っている → これが DPI を反映させる操作か。
- 実験 E5 (要了承、`examples/g600res.rs`):
  1. 保存済み DPI と効いている DPI がずれた状態を作る。
  2. 待機中 (bit3=1) を確認してから解像度 SET を 1 回送る。
  3. 0xF2 で、効いている DPI が保存値に変わるかを見る。
- **E5 結果 (2026-11-19)**:
  - 保存済み段 1 = 400 dpi、効いている DPI = 8200 の状態で `[F0 40 00 00]` を 1 回送った (待機中を確認してから)。
  - 0xF2 は `A4` (8200) から `08` (400) に変わり、体感でも感度が下がった。管理者権限は不要。
  - → 保存後とプロファイル切替後に、既定段で解像度を再設定する実装を追加した (`g600.rs` の `apply_resolution`)。

---

## 4. Phase 0 — 診断 example (完了)

- `examples/g600probe.rs`: **読み取り専用**。コレクション一覧・レポート記述子・0xF0-F6 の GET とデコード。
  `--roundtrip` は事故 (§3.5) を受けて削除済み。
- `examples/g600select.rs`: 0xF0 `[F0, 80|(p<<4), 0, 0]` でアクティブプロファイルを切り替えるだけの復旧ツール。
- `examples/g600e1.rs`: 非アクティブの F4 にだけ元データを書く実験ツール (`check` / `write` / `verify` / `e4`)。
- バックアップ: `.private-analysis/g600/F3/F4/F5_original_2026-11-19.txt` (Git 管理外)。
- これらの example は復旧用に残す。Phase 1 で変更しない。

---

## 5. Phase 1 — ドライバ本体 (方針確定 2026-11-19)

### 5.0 方針 (ユーザー決定)

- **明示保存**: 画面での変更はドライバ内の「保存待ち」コピーにだけ反映する。
  「本体に保存」アクションで 1 回だけ 154B を書く (フラッシュ書き込み回数と事故リスクを最小化)。
- **使用中のプロファイルには直接書かない**:
  1. 保存時は、一時的に別のプロファイルへ切り替えてから対象を書く (E1 / E4 で成功した条件)。
  2. 書き終わったら元のプロファイルに戻す。
  - 保存中は `HubEvent::Progress` で「保存中… マウスに触らないでください」を出す (入力の無効化はしない)。
- 使ってよい SET は次の 3 つだけ:
  - プロファイル選択 `[F0, 80|(p<<4), 0, 0]` (`g600select` / E4 で実績あり)
  - 154B のプロファイル書き込み
  - 解像度の再設定 `[F0, 40|(r<<1), 0, 0]`
    - 当初は効果未確定で禁止していたが、E5 (§3.9) で保存済み DPI を即時反映することを確認した。
    - 使うのは「使用中プロファイルの保存後」と「プロファイル切替後」に、既定段 (offset 13) を指定するときだけ。
    - 無効な段 (DPI 0) は送らない。送信後は 0xF2 で反映を照合する。
    - 照合に失敗しても保存は成功扱いにし、挿し直しを案内する。
- 実機は廃盤・最後の 1 台。**0xF1 / F2 / F6 への SET、上記以外の report への SET は禁止**。

### 5.1 変更するファイル

| ファイル | 変更 |
|---|---|
| `devhub-core/src/model.rs` | 以下を**追加**する。<br>- `DriverKind::LogitechG600`<br>- `ButtonAction::GShift`、`ButtonActionKind::GShift` (`kind()` に 1 行)<br>- `ActionSpec.commit: bool` (既定 false) と builder `.commit()`<br>- `DeviceState.unsaved: bool` (`#[serde(default)]`) |
| `DeviceState { .. }` を直接組み立てている全箇所 (struct 定義を除き約 16 か所: demo.rs・各ドライバ・テスト) | `unsaved: false` を足すだけ (機械的変更) |
| `devhub-core/src/registry.rs` | G600 の 1 行: `vid 0x046D, pid 0xC24A, usage_page Some(0xFF80), usage Some(0x80)`、Mouse、vendor "Logitech"、model "g600"、Wired、`DriverKind::LogitechG600`、`verified: false` |
| `devhub-core/src/demo.rs` | demo マウスの `button_kinds` に `GShift` を追加 |
| `devhub-hid/src/transport.rs` | 番号付き feature report の送受信を追加 (§5.3) |
| `devhub-hid/src/g600.rs` | **新規** ドライバ。`pub fn open(dev, info, events: Option<Watcher>)` |
| `devhub-hid/src/lib.rs` | `mod g600;`、`driver_ready`、`make_driver` の分岐 (`events` を渡す) |
| `device-hub/src/overlay.rs` / `pages/mouse.rs` | GShift の表示名・既定値・説明文 (各 1 行) |
| `device-hub/src/pages/system.rs` | `DriverKind` の表示名 "Logitech G600 (オンボード)" |
| `device-hub/src/pages/lighting.rs` | `brightness_max == 0` の扱い 2 か所 (§5.5) |
| `device-hub/src/pages/device.rs` 付近 | 未保存バナー (§5.5) |
| 既存ドライバ (`atk.rs` `lamzu.rs` `mousefit.rs` `mousefit8.rs`) | Button の match に `GShift => Err(Unsupported)` を追加 |
| `docs/PROTOCOL_NOTES.md` / `docs/ADD_DEVICE.md` | G600 の節と対応表の行。注意書き (G HUB / LGS 併用、Dynamic Lighting、明示保存、復旧ツール) |

### 5.2 caps

- `profiles`: `count: 3, renamable: false`
- `settings`: `SettingSpec::choice("polling", PERFORMANCE, "ポーリングレート", [125,250,500,1000])` のみ (値は Hz)。
- `actions`:
  - `ActionSpec::new("save_onboard", SYSTEM, "本体に保存").commit()`
    - help:「未保存の変更を G600 に書き込みます。保存中 (数秒) はマウスに触らないでください。」
  - `ActionSpec::new("discard", SYSTEM, "変更を破棄")`
    - help:「未保存の変更を捨てて、本体の内容を読み直します。」
- `mouse`:
  - DPI: `dpi_min 200, dpi_max 8200, dpi_step 50, min_stages 1, max_stages 4, xy_split false, stage_colors false, stage_toggle false`
    - 無効ステージ (0) は「ステージを減らす」で表現する。
  - `buttons`: 40 枠。0-19 = G1-G20、20-39 = 「G-Shift + G1」…「G-Shift + G20」。
    - 名前: G1 左クリック / G2 右クリック / G3 ホイールクリック / G4 ホイール左 / G5 ホイール右 /
      G6 / G7 / G8 / G9-G20 (サイド 12 ボタン)。
  - `button_kinds`: `Disabled, Mouse, Dpi, Key, ProfileCycle, GShift`
  - `require_left_click: true`、`macros: None`、`live_buttons: vec![]`、`adaptive_dpi: None`
- `lighting`: 2 エリア。エフェクト id は offset 4 の生値。速度の無いエフェクトには `.still()`。
  - id 0「本体」:
    - effects = `new(0,"固定").color().still()` / `new(1,"ブリージング").color()` / `new(2,"カラーサイクル")`
    - `speed_max 14`、`brightness_max 0`、`switchable false`
  - id 1「G-Shift 中」:
    - effects = `new(0,"固定").color().still()`
    - `speed_max 0`、`brightness_max 0`、`switchable false`
- `battery: false`

### 5.3 transport 追加 API

`Channel` に追加する (Mock 実装も。mock の reply は ID 込みのバイト列):

- `get_feature_id(&self, rid: u8, len: usize) -> HubResult<Vec<u8>>`
  - 256B バッファで GET し、先頭 `len` バイト (ID 込み) を返す。`n < len` は `Protocol` エラー。
- `send_feature_id(&self, data: &[u8]) -> HubResult<()>` (`data[0]` が report ID)

既存の `send_feature` / `get_feature` は変えない。

### 5.4 ドライバの振る舞い (`g600.rs`)

内部状態 (`Mutex`):

- `device: [[u8;154]; 3]` — 本体から読んだ内容 (保存済みの正)
- `pending: Option<(u8, [u8;154])>` — 保存待ちの (プロファイル番号, 内容)。None = 未変更
- `opened_at: Instant`

#### 1. open

- 0xF0 と F3/F4/F5 を **GET だけ**で読み、`device` に入れる。
- `opened_at` を記録する。open 時に待ちは入れない。待ちは書き込み時に §5.4-5 で保証する。

#### 2. read()

1. 0xF0 を GET して `active` を得る (profile = `(b1>>4)&0x0F`。>2 は `Protocol`)。
2. 対象プロファイルを決める。
   - `pending` があれば、`pending` のプロファイルと内容を使う。
   - 無ければ `device[active]` を使う。ただし未変更なら、そのプロファイルを GET し直して `device` を更新する
     (本体のボタンでプロファイルが変わった場合に追従する)。
3. `DeviceState` を組み立てる。
   - `unsaved` = `pending.is_some()`。
   - stages は 0 でないスロットを詰める。`active_stage` は既定ステージ (offset 13 − 1) を詰めた後の index。
   - lighting: `on` は常に true、`brightness` は 0、`speed = 15 − 周期` (周期 0 は 14)。
   - ボタン 40 個は §3.2 の表で変換する。

#### 3. apply() — **HID には書かない** (例外は `Profile`)

- `pending` が無ければ `device[active]` のコピーから作り、Change を反映する (RMW)。
  - 反映後に内容が `device[p]` と同じに戻ったら、`pending = None` にする。
  - 未保存の状態が変わったとき (なし→あり、あり→なし) だけ `HubEvent::StateChanged` を出し、
    UI に read させてバナーを更新する。
- 各 Change:
  - `Setting{"polling"}`: offset 11。4 値以外は `Invalid`。
  - `DpiStages{stages, active}`:
    - 0 個・5 個以上・active 範囲外は `Invalid`。
    - 先頭から詰めて書き、残りは 0。値は 50 の倍数に丸め、200-8200 にクランプする。
    - offset 13 = active+1。
  - `ActiveStage(i)`: offset 13 = i+1 (既定ステージ)。保存後、プロファイル再選択で有効になる。
  - `Button{slot, action}`:
    - slot 0-19 は offset 31+slot×3、20-39 は offset 94+(slot−20)×3、40 以上は `Invalid`。
    - **左クリック保護**: 変更後の slot 0-19 に `Mouse(Left)` が無くなるなら `Invalid`
      (「G-Shift でない側 (G1–G20) に左クリックが 1 つ必要です」)。
    - `Dpi(Shift(v))` は offset 12 = v/50 も更新する (他の 0x15 ボタンがあれば `StateChanged`)。
    - Mouse は Left/Right/Middle/Back/Forward のみ。Key の usage > 0xFF は `Invalid`。
    - `Unsupported{ty, params}` はそのまま書き戻す。
  - `Lighting{0}`: offset 1-3 = 色、offset 4 = effect、offset 5 = `15 − clamp(speed, 0, 14)`。
  - `Lighting{1}`: offset 91-93。
  - `Profile(p)`:
    - `pending` があれば `Invalid` (「未保存の変更があります。保存するか破棄してから切り替えてください」)。
    - 無ければ §5.4-5 の SET 手順で `[F0, 80|(p<<4), 0, 0]` を送り、bit3=1 を待つ。
  - その他は `Unsupported`。

#### 4. run_action

- `"discard"`: `pending = None`。0xF0 と F3-F5 を GET し直し、`ActionOutcome{ reload: true }` を返す。
- `"save_onboard"`: `pending` が無ければ何もしない。あれば以下の順で行う。
  1. `Progress("保存中… マウスに触らないでください", None)` を出す。
  2. 対象 `p` = `pending` のプロファイル、`data` = `pending` の内容。
     0xF0 を GET して、現在アクティブな `a` を得る。
  3. **退避先 `q` を決める** (`p == a` のときだけ必要):
     - `p` 以外で、`device[q]` の G1-G20 に `Mouse(Left)` があり、DPI 段が 1 つ以上有効なプロファイル。
     - 番号の小さい方を優先する。無ければ書かずに `Protocol` エラー
       (「退避できるプロファイルがありません」)。
  4. `p == a` なら `q` を選択する (§5.4-5 の SET 手順)。
  5. `p` に `data` を書く (§5.4-5 の SET 手順)。
  6. GET `p` で照合する。不一致なら 1 回だけ再書き込みする。
     - それでも不一致なら `p` は戻さず (`q` のまま) `Protocol` エラーで止める
       (「保存に失敗しました。プロファイル q+1 に退避しています」)。
  7. `p == a` だった場合は `p` を選択し直す (§5.4-5)。0xF0 を GET し、active==p と bit3==1 を確認する。
  8. **反映チェック** (`p == a` のときのみ):
     - 0xF1 を GET し、`F1[4]` (effect) と `F1[5]` (周期) を `data[4]` / `data[5]` と比べる
       (0xF1 = 現在の LED 状態という推定に基づく)。
     - 不一致なら、保存内容のずれを疑って `q` を選択し直し、`Protocol` エラー
       (「保存内容の確認に失敗しました。プロファイル q+1 に退避しています。USB を挿し直して確認してください」)。
  9. 成功したら `device[p] = data`、`pending = None` にし、`Progress done` を出して
     `ActionOutcome{ message: "G600 に保存しました", reload: true }` を返す。
  - どの段階で失敗しても `Progress done` を必ず出す。
  - **失敗時にマウスが「左クリックの無いプロファイル」のまま残らないこと**を最優先にする。

#### 5. SET 手順 (全 SET 共通。§3.8)

1. `opened_at` から 2 秒経っていなければ、残り時間を待つ。
2. 0xF0 を 100ms 間隔で GET し、bit3==1 を待つ (最大 3 秒。超えたら送らずに `Protocol` エラー)。
3. SET を 1 回送る。
4. 300ms 待ち、0xF0 だけを 100ms 間隔で GET して bit3==1 を待つ (最大 3 秒)。
   **この間は他の SET を一切送らない**。超えたら `Protocol` エラー。

#### 6. エラー / info

- I/O は `Io`。切断は `Disconnected` (`mousefit8.rs:427` 付近、`walkplay.rs:154` 付近の判定に合わせる)。
- info: firmware / battery は `None`。

### 5.5 UI の変更

- GShift の表示名は「G-Shift (押している間 別割り当て)」。
  説明文は「押している間、G-Shift 側 (G-Shift + Gn) の割り当てに切り替わります。」。
- `lighting.rs` (caps による出し分け):
  1. `brightness_max == 0` なら明るさ行を出さない。
  2. プレビューの明るさ係数 (現 749 行付近) は `brightness_max == 0` のとき 1.0 にする。
- **未保存バナー** (`DeviceState.unsaved == true` かつ caps に `commit` アクションがあるとき):
  - デバイス画面の上部に「本体に未保存の変更があります」と、そのアクションの実行ボタン (ラベルは ActionSpec のもの) を出す。
  - 実行中は既存の Progress 表示 (`pages/device.rs` の sync 表示) をそのまま使う。
  - デバイス名・アクション id で分岐しない (`commit` フラグだけを見る)。
- ボタン図の番号マークは 5 個まで。ボタン画面の「既定に戻す」問題はスコープ外。

### 5.6 テスト (fixture はレイアウト表から手で作る。参考リポジトリや実機バックアップのバイト列は使わない)

`Channel::mock()` で行う:

- decode/encode の往復一致 (不明バイト 6-10, 18-30 の保持)。ボタン変換表の全行。未知 code の往復。
- DPI ステージの詰め/展開・`Invalid` 条件。Shift・Polling・LED 速度の変換。左クリック保護。
- **apply は一切 SET を送らない** (mock の sent が空)。`unsaved` の立ち下がり (元に戻すと None)。
- `Profile` は `pending` ありで `Invalid`、なしで 0xF0 SET 1 回 (bit3 待ちを挟む)。
- `save_onboard` (p == active):
  - 送信順が「q 選択 → p 書き込み → p 選択」になる。各 SET の前後で 0xF0 の GET が挟まる。
  - q は「左クリックありのプロファイル」から選ばれる。
  - 候補が無ければ何も送らずにエラーになる。
- `save_onboard` (p != active): 選択 SET を送らず、p の書き込みだけを行う。
- 照合不一致 → 再書き込み 1 回 → それでも不一致なら q のままエラー (p 選択を送らない)。
- 0xF1 の effect/周期不一致 → q を選び直してエラー。
- bit3 が 3 秒 1 にならない → SET を送らずにエラー。
- 0xF0 の bit 解釈: `0x19` → p1/r0、`0x26` → p2/r3、`0x30` → `Protocol`。
- 0xF0/F1/F3/F4/F5 以外の report ID への SET がコード上に存在しないことをテストかレビューで確認する。

### 5.7 Phase 1.1 — UI の改善 (実機で触った指摘、2026-11-19)

実機 (読み取りのみ) で触ったユーザーの指摘を受けた改善。**保存の実機テストはこの後に行う**。

#### 1. 保存ボタンを常に表示する (レイアウトのガタつき防止)

- 現状は `pages/device.rs` の「本体に未保存の変更があります」行が出たり消えたりして、DPI 操作のたびに画面がずれる。
- 変更後:
  - caps に `commit` アクションがあるデバイスでは、**保存ボタンだけを常に同じ位置に出す** (メッセージ文は出さない)。
  - 位置はデバイスヘッダーの右側など、本文の高さに影響しない場所にする。
  - 押せる条件は `DeviceState.unsaved == true` かつ実行中 (Progress 表示中) でないこと。
  - 押せないときは非活性表示にする (不透明度を下げる、クリックハンドラなし、カーソルは通常)。
- `ui.rs` に押せない状態のボタン表現が無ければ、最小限の追加で作る。既存ボタンの見た目は変えない。

#### 2. マウス図の種類を caps で選べるようにする

- 契約 (`model.rs`) に**追加**する:
  ```rust
  #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
  pub enum MouseFigure { #[default] Standard, Mmo12 }
  ```
  と `MouseCaps.figure: MouseFigure`。
- 既存の `MouseCaps { .. }` 全箇所 (demo.rs、lamzu.rs (core)、atk.rs、mousefit.rs、mousefit8.rs) には
  `figure: MouseFigure::Standard` を足すだけ。G600 は `Mmo12`。
- `pages/mouse.rs` の `page_buttons`:
  - `Standard` は**現在の図をそのまま使う** (変更しない)。
  - `Mmo12` は新しい図を描く。番号はレイヤー内の index + 1 (= G 番号)。ホバー連動は既存と同じ仕組みにする。
    - 上面図: G1 左クリック、G2 右クリック、G6 は G2 のさらに右の 3 つ目のクリック、
      G3 ホイール (G4 / G5 はホイールの左右チルト)、G7 / G8 はホイール後方の上面ボタン。
    - サイド: 3 列 × 4 行のグリッド。上の行から G9-G11、G12-G14、G15-G17、G18-G20 (行内は前→後)。
      正確な配置は後でユーザーが実機と照合して直すので、見やすさ優先でよい。
- UI はデバイス名で分岐しない (`figure` だけを見る)。

#### 3. ボタンのレイヤー (通常 / G-Shift) をタブで切り替える

- 契約に**追加**する:
  ```rust
  #[derive(Clone, Debug, PartialEq)]
  pub struct ButtonLayer { pub name: String, pub help: Option<String> }
  ```
  と `MouseCaps.layers: Vec<ButtonLayer>`。
  - 空 = 単一レイヤー (現状どおり)。既存の全箇所は `layers: vec![]`。
  - 非空のとき、`buttons.len()` はレイヤー数で割り切れ、slot = layer × per_layer + i とする。
- G600:
  - `layers` = `[{ name: "通常", help: None }, { name: "G-Shift", help: Some(…) }]`
    - help:「G-Shift ボタン (既定は G6) を押している間だけ有効な割り当てです。押している間は LED が「G-Shift 中」の色になります。」
  - ボタン名は 20-39 も 0-19 と同じ名前にする (「G-Shift + 」の接頭辞を外す。どのレイヤーかはタブで分かる)。
- `pages/mouse.rs`:
  - layers が 2 つ以上なら、ボタン一覧の上にタブを出す。キーボードのレイヤー切替など既存のセグメント UI を再利用する。
    選択中レイヤーの help をタブの下に表示する。
  - 一覧と図には、選択中レイヤーの per_layer 個だけを出す。`Change::Button` の slot は全体 index で送る。
  - 選択中レイヤーは `Shell` に持ち、デバイスを切り替えたら 0 に戻す。
  - 「既定から変更済み」の判定 (先頭 5 個) はレイヤー 0 のときだけ行う。
  - 「既定に戻す」の挙動は変えない (スコープ外)。
- index 計算 (レイヤー → 全体 slot) は小さな純関数にして `device-hub` のテストに足す。

#### 受け入れ条件 (Phase 1.1)

- WSL の check / clippy / test 条件は §7 と同じ (警告を増やさない、全テスト成功)。
- 既存機種 (demo マウス含む) のボタン画面・図・デバイス画面の見た目が変わらない。保存ボタンも出ない。
- G600 (ユーザーが Windows で確認):
  - 保存ボタンが常に同じ位置にあり、未保存のときだけ押せる。DPI 操作で画面がずれない。
  - ボタン画面に「通常 / G-Shift」タブと help があり、20 個ずつ表示される。
  - G600 型の図に G1-G20 の番号が出て、ホバーで一覧と連動する。

### 5.8 Phase 1.2 — G600 図を参考画像に合わせる (2026-11-19)

ユーザーが参考の配置図を置いた: `docs/g600_flat_button_diagrams_v2.svg` (`.png` は同じ内容の画像)。
**→ §5.9 で新しい参考画像に置き換え。配置は §5.9 が正。**
現在の `Mmo12` 図 (`pages/mouse.rs` の `mmo12_outline` と座標表) は四角いパネルと丸い番号だけなので、この配置図に寄せて描き直す。

- **上面図** (SVG の上パネル):
  - 胴体の輪郭の中に、3 つのクリックパドルを独立した領域として描く: G1 (左)、G2 (右)、G6 (G2 のさらに右の細長いパドル)。
    **G6 に「G-Shift」の注記は付けない** (G-Shift をどのボタンに割り当てるかは利用者次第のため。ユーザー指示 2026-11-19)。
  - G3 はホイールで、G1 / G2 の境目にまたがる。
  - **G4 / G5 はクリック部の下に横並びの独立ボタン** (左右の矢印つき)。ホイールの左右ではない。
  - **G7 / G8 はその下に縦に 2 つ**並ぶ。
- **左側面図** (SVG の下パネル): 側面のシルエットの中に 3 列 × 4 段のグリッド
  (上の段から G9-G11、G12-G14、G15-G17、G18-G20)。
- 表現:
  - 丸い番号マークではなく、**各ボタンの領域そのもの**にラベル (G1…) を描く。
  - ホバー連動 (一覧の行にマウスを乗せる → 図の該当ボタンを強調) は、領域の塗りを accent にして示す。
  - 色はテーマ (`theme::pal()`) から取る。SVG の色は参考にとどめ、ライト / ダークの両テーマで読めること。
  - 寸法は SVG の座標を等倍率で縮めて使ってよい。側面のグリッドは小さくなりすぎないこと
    (キー 1 つの高さ 18px 以上、ラベルが読めること)。そのため上面と側面で縮尺が違ってよい。
  - 図全体の幅・高さは、ボタン一覧との並びが崩れない範囲で今より大きくしてよい。
  - 曲線の輪郭は、角丸の div の組み合わせで近似してよい。GPUI の canvas / path を使う場合は既存コードの使い方に合わせる。
- `Standard` の図は変えない。図の種類 (`MouseFigure`) 以外で分岐しない。
- 参考画像 (`docs/g600_button_layout.*`、旧 `g600_flat_button_diagrams_v2.*`) は変更・削除しない。
  - **TODO (ユーザー指示)**: G600 対応がすべて終わったら、参考画像 (`.png` / `.svg`) は削除する (コミットしない)。

受け入れ条件: §7 の WSL 条件に加え、ユーザーが Windows で両テーマの見た目と、ホバーの連動を確認する。

### 5.9 Phase 1.3 — 新しい参考画像で G600 図を直す (2026-11-19)

ユーザーが参考画像を更新した。
- **理想形は `docs/g600_button_layout.png`**。
- `docs/g600_button_layout.svg` は座標の目安で、png とずれがある。食い違ったら png を正とする。
- §5.8 からの主な変更点:

- **上面図** (ケーブル側が上):
  - G1・G2・G6 は 3 つのクリック部。G6 は G2 の右隣 (png では G1 / G2 よりやや小さい)。
  - **G3 はホイール中央**。ホイールは縦長で、G1 / G2 の間に入る。
  - **G4 / G5 はホイール下部の真横** (左に「◀ G4」、右に「G5 ▶」)。クリック部の下ではない。
  - ホイールの下に **G8、その下に G7** (縦並び、G8 が上)。
  - 胴体の輪郭は png の多角形 (角ばった肩と、下すぼまりのお尻) に寄せる。
- **左側面図** (ケーブル / 前が左。左側に「← ケーブル / 前」の注記):
  - **4 列 × 3 段**。列は前 (左) → 後ろ (右)、各列は下から上へ番号が増える。
    - 1 列目: 下 G9 / 中 G10 / 上 G11
    - 2 列目: G12 / G13 / G14
    - 3 列目: G15 / G16 / G17
    - 4 列目: G18 / G19 / G20
  - シルエットは png の側面形 (前が低く、後ろが盛り上がる) に寄せる。
- 変えないこと:
  - G6 に「G-Shift」の注記は付けない (§5.8)。
  - ボタン領域そのものにラベルを描き、ホバーで accent に塗る表現は維持する。
  - テーマ対応、Standard 図の不変、`MouseFigure` 以外で分岐しないことも維持する。
- 図の見出し (上面図 / 左側面図) は今の `ui::faint` のままでよい。png のピル型見出しや背景グラデーションは再現しなくてよい。

## 6. 禁止事項 / 変えてはいけないもの

- 契約 (`model.rs`) は**追加のみ**。既存 variant・フィールドの変更・削除・並べ替えは禁止。
- UI でデバイス名・VID/PID・アクション id による分岐をしない。
- 既存ドライバのプロトコル処理に手を入れない (GShift の 1 行と `unsaved: false` のみ)。
- `transport.rs` の既存メソッドの挙動を変えない。
- **G600 への SET は「0xF0 のプロファイル選択」と「0xF3-F5 の 154B」以外を実装しない**。
  - 0xF0 の解像度 SET、0xF1 / F2 / F6 への SET は禁止。
- 参考 Python リポジトリのコード・バイト列、`.private-analysis/` の実機バックアップをコードやテストに転載しない。
- `examples/g600*.rs` (復旧ツール) を変更しない。
- `vendor/` は触らない。リポジトリに `.cargo/config.toml` を作らない。`Cargo.lock` の変更をコミットしない。
- 不明バイトをゼロ埋めで上書きしない (必ず RMW)。

## 7. 受け入れ条件

- [ ] (WSL) 静的チェックとテスト:
  - `cargo check --workspace` が通る。
  - `cargo clippy -p devhub-core -p devhub-hid` の lib 警告は 0 件。
  - `cargo clippy -p device-hub` は基準値から増えていない。
  - `cargo test --workspace` が全件成功する。
- [ ] (Windows・ユーザー) `DEVHUB_DEMO=1` でデモマウスに G-Shift を選べる。既存機種の画面に未保存バナーが出ない。
- [ ] (Windows 実機・ユーザー。**初回は Claude と手順を確認してから**) G600 で次を確認する:
  - 読み取りだけで全画面が正しく表示される (3 プロファイル切替・DPI・ボタン 40・LED)。
  - 変更すると未保存バナーが出て、「変更を破棄」で消える。この間 HID への書き込みは発生しない。
  - 「本体に保存」が成功し、USB を挿し直しても変更が保持されている。
    最初の保存は**非アクティブなプロファイル**で試し、次にアクティブなプロファイルで試す。
- [ ] `docs/PROTOCOL_NOTES.md` に G600 節、`docs/ADD_DEVICE.md` の対応表に行がある。`verified` は実機確認後に true にする。

## 8. テストコマンド

```bash
# WSL (Codex)
cargo check --workspace
cargo clippy -p devhub-core -p devhub-hid
cargo clippy -p device-hub
cargo test --workspace

# Windows (ユーザー)
$env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\devhub-target"   # WSL 上のフォルダからビルドするとき
cargo run -p device-hub
```

## 9. リスク

- **保存中にマウスを操作される**: 入力は無効化しない。数秒間は退避先プロファイルの割り当てになる。
- **0xF1 の解釈が推定**: 反映チェックが誤検知した場合は、退避先プロファイルに切り替わって止まるだけ (安全側)。
- **保存失敗時は退避先プロファイルのまま**: 復旧はアプリの再保存か、`examples/g600select` / G HUB。
- **アクティブ段の即時切替は非対応**: 解像度 SET を使わないため。既定段の変更は保存後に有効になる。
- **未保存の変更はアプリ終了・切断で失われる** (バナーで告知)。
- **キーボード page の音量キー**: Windows では効かない可能性が高い (Media は宣言しない)。
- **G HUB / LGS / Windows Dynamic Lighting の併用**: HID の取り合いや LED の上書き。docs に注意書きを入れる。
- **DPI シフト値はプロファイルに 1 つ**: シフトボタンが複数あると全て同じ値になる。
- **左クリック保護の二重判定**: UI (40 枠全体) とドライバ (G1-G20) で食い違うのは仕様として受け入れる。
