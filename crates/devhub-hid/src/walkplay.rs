//! WalkPlay CB 系プロトコル (peq.szwalkplay.com) — TP55 等の USB DAC ドングル向け。
//!
//! 物理レイヤー:
//! - consumer control IF (up=0x000C) 上の **report 0x4B** (IN/OUT とも 64B)。
//! - 読み取り: `[0x80, sub, params...]` → 応答 `[0x4B, 0x80, sub, params…, data…]`
//!   (応答は先頭 2 バイトがコマンドエコー、データは r[3..])。
//! - 書き込み: `[0x01, sub, params...]` (応答無し)。保存は `[1,1,0]` (refereshToFlash)。
//! - ⚠ `[1,255]` は**ブートローダ移行** (0666:0888 "USB20" 救砖モード) — 絶対に送らない。
//!
//! サブコマンド (peq.szwalkplay.com CB クラス + 実機プローブで確認):
//! - `[80,0c]` → fw バージョン ASCII
//! - `[80,09]` → EQ フレーム (tag+1バンド。`[80,9,0,0,band]` でバンド別読み取り)
//! - `[80,02]` → マイクオフセット (e[3..4] int16 LE、UI 値 = raw*15/32767)
//! - `[80,03]` → EQ ボリューム (e[4]、UI 値 -16..+6 = raw-16)
//! - `[80,0e]` → 4B (シリアル/機能フラグ?。記録のみ)
//! - `[80,11]` → DAC フィルター index (e[3]-1 → 0..4)
//! - `[80,16,2,ch,0]` → チャンネルバランス (e[3]=ch, e[5]=signed8 減衰値)
//! - `[80,19]` → フィルター出力ゲインモード (e[3]+1 → 1..3)
//! - `[80,1d]` → DAC ワークモード (e[3] → 0/1)
//! - `[80,18]` / `[80,1e]` → 値のみ確認 (意味未確定。記録のみ)
//! - タイムアウト (TP55 非対応): ENC `[80,1b]`、フィルターバンク `[80,25,1,n]`、
//!   `[80,22,24,26]`、`[80,d3]`、`[80,da]`
//!
//! DAC EQ (公式 s2/CB1300D クラスと実機で確認):
//! - 書き込み: バンドごとに 36B フレーム `[1,9,24, 0,band, 0,0, <係数20B>,
//!   freq u16LE, q*256 u16LE, gain*256 i16LE, type, 0(offset), tag]`
//! - 係数: biquad [b0,b1,b2,-a1,-a2] 各 i32 LE。**96kHz で計算** (実機応答と
//!   バイト一致)。PK/LS/LP/HP は Q30、HS のみ公式 X1 クラスに倣い **Q27**
//!   (±(2^27-1) でクランプ、gain×0.85 ヘッドルーム、Q は 0.5..5 にクランプ)。
//! - 適用: 全フレーム送信 → `[1,10,4,0,0,255,255]` (refereshToMemery)
//! - 保存: さらに `[1,1,0]` (refereshToFlash)。公式 saveEqInfo と同じ順序。
//! - 読み取り: `[80,9,0,0,band]` → 応答 = `[4B,80,09]` + param エコー
//!   `[0,0,band]` + 書き込みフレーム v[5..35] (実機ダンプで確認:
//!   生 [8..27]=係数, [28..29]=freq, [30..31]=q*256, [32..33]=gain*256,
//!   [34]=type, [35]=offset, [36]=tag)。
//! - type index: LS=1 PK=2 HS=3 LP=4 HP=5 (getFilterIndexs)。

use devhub_core::backend::*;
use devhub_core::model::*;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::transport::Channel;

const RID: u8 = 0x4B;
const POLL_MS: i32 = 60;
const DEADLINE_MS: u64 = 400;
const FAIL_FAST_AFTER: u32 = 4;
const PROBE_MS: u64 = 1500;
const PROBE_WAIT_MS: u64 = 300;

mod sub {
    pub const MIC_OFFSET: u8 = 0x02;
    /// EQ タブの Volume スライダー (公式 setDacOffset → [1,3,2,0,v])。
    /// MIC タブの L/R バーは VU メーター (視覚化) で設定項目ではない。
    pub const EQ_VOLUME: u8 = 0x03;
    /// DAC EQ バンドフレームの読み書き。
    pub const EQ: u8 = 0x09;
    /// 書き込んだ EQ をレジスタへ即時適用 (refereshToMemery)。
    pub const APPLY: u8 = 0x0A;
    pub const FW_VER: u8 = 0x0C;
    pub const FILTER_IDX: u8 = 0x11;
    pub const BALANCE: u8 = 0x16;
    pub const GAIN_MODE: u8 = 0x19;
    pub const WORK_MODE: u8 = 0x1D;
}

/// DAC デジタルフィルター (公式 DAC Filter タブの 5 択。実機で確認: index 2 = SLOW-LL)。
const FILTERS: [&str; 5] = ["FAST-LL", "FAST-PC", "SLOW-LL", "SLOW-PC", "NON-OS"];

/// PeqFilter → wire の type index (公式 getFilterIndexs: LS=1,PK=2,HS=3,LP=4,HP=5)。
fn wire_filter(f: PeqFilter) -> u8 {
    match f {
        PeqFilter::LowShelf => 1,
        PeqFilter::Peak => 2,
        PeqFilter::HighShelf => 3,
        PeqFilter::LowPass => 4,
        PeqFilter::HighPass => 5,
    }
}

fn from_wire_filter(t: u8) -> PeqFilter {
    match t {
        1 => PeqFilter::LowShelf,
        3 => PeqFilter::HighShelf,
        4 => PeqFilter::LowPass,
        5 => PeqFilter::HighPass,
        _ => PeqFilter::Peak,
    }
}

pub fn open(dev: hidapi::HidDevice, info: DeviceInfo) -> HubResult<WalkplayDriver> {
    dev.set_blocking_mode(false).ok();
    let mut d = WalkplayDriver {
        ch: Arc::new(Channel::open(dev, 65, 65)),
        caps: Mutex::new(None),
        fails: AtomicU32::new(0),
        dead_since: Mutex::new(None),
        eq_tag: Mutex::new(0),
        save_gen: Arc::new(AtomicU64::new(0)),
        saving: Arc::new(AtomicBool::new(false)),
        info,
    };
    // firmware を取得して名札に入れる
    if let Ok(r) = d.cmd(&[0x80, sub::FW_VER]) {
        let end = r.iter().rposition(|&b| b != 0).map(|i| i + 1).unwrap_or(0);
        let fw: String = r[..end]
            .iter()
            .filter(|b| b.is_ascii_graphic())
            .map(|&b| b as char)
            .collect();
        if !fw.is_empty() {
            d.info.firmware = Some(fw);
        }
    }
    Ok(d)
}

pub struct WalkplayDriver {
    ch: Arc<Channel>,
    caps: Mutex<Option<Capabilities>>,
    fails: AtomicU32,
    dead_since: Mutex<Option<Instant>>,
    /// 最後に読んだ EQ のタグ (応答 e[35])。書き込みフレーム v[35] にそのまま入れる。
    eq_tag: Mutex<u8>,
    /// フラッシュ保存の予約世代。`save()` のたびに +1、ワーカーは世代が
    /// 1 秒間変わらなければ 1 回だけ `[1,1,0]` を送る (連続変更を合体)。
    save_gen: Arc<AtomicU64>,
    saving: Arc<AtomicBool>,
    info: DeviceInfo,
}

impl WalkplayDriver {
    /// `[0x80, sub, params...]` → 応答データ (r[3..]、エコーの後)。
    /// タイムアウト時は 40ms 後に 1 回だけ再送する (書き込み直後に稀に
    /// 応答が 1 回落ちる実機癖への対処)。
    fn cmd(&self, req: &[u8]) -> HubResult<Vec<u8>> {
        match self.cmd_once(req) {
            Err(HubError::Protocol(_)) => {
                std::thread::sleep(Duration::from_millis(40));
                self.cmd_once(req)
            }
            r => r,
        }
    }

    fn cmd_once(&self, req: &[u8]) -> HubResult<Vec<u8>> {
        let probing = self.fails.load(Ordering::Relaxed) >= FAIL_FAST_AFTER;
        if probing {
            let mut ds = self.dead_since.lock().unwrap_or_else(|p| p.into_inner());
            match *ds {
                Some(t) if t.elapsed() < Duration::from_millis(PROBE_MS) => {
                    return Err(HubError::Disconnected)
                }
                _ => *ds = Some(Instant::now()),
            }
        }
        self.ch.send_report(RID, req)?;
        let wait_ms = if probing { PROBE_WAIT_MS } else { DEADLINE_MS };
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        while Instant::now() < deadline {
            match self.ch.recv(POLL_MS) {
                Ok(b) if b.first().copied() == Some(RID) => {
                    // 応答は先頭 2B [0x80,sub] のエコー。パラメータ部は応答側で
                    // 値に置き換わることがある (ch/値フィールド) ので先頭だけ見る。
                    if b.len() > 3 && b[1] == req[0] && b[2] == req[1] {
                        self.fails.store(0, Ordering::Relaxed);
                        *self.dead_since.lock().unwrap_or_else(|p| p.into_inner()) = None;
                        return Ok(b[3..].to_vec());
                    }
                }
                Ok(_) => {}
                Err(HubError::Protocol(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let f = self.fails.fetch_add(1, Ordering::Relaxed) + 1;
        if f >= FAIL_FAST_AFTER {
            *self.dead_since.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
            return Err(HubError::Disconnected);
        }
        Err(HubError::Protocol("walkplay: response timeout".into()))
    }

    /// 読み取り `[0x80, sub, params...]` → e[2..] (`cmd` は r[3..] を返すので
    /// 公式ディスパッチャの `e[n]` は `data[n-2]`)。
    fn query(&self, sub: u8, params: &[u8]) -> HubResult<Vec<u8>> {
        let mut req = vec![0x80, sub];
        req.extend_from_slice(params);
        self.cmd(&req)
    }

    /// 読み取り → e[3] の u8 (= data[1])。
    fn query_u8(&self, sub: u8, params: &[u8]) -> HubResult<u8> {
        Ok(self.query(sub, params)?.get(1).copied().unwrap_or(0))
    }

    /// 書き込み `[0x01, sub, params...]`。応答は待たない (無い)。
    fn write(&self, sub: u8, params: &[u8]) -> HubResult<()> {
        // fail-fast 中は送らない
        if self.fails.load(Ordering::Relaxed) >= FAIL_FAST_AFTER {
            return Err(HubError::Disconnected);
        }
        let mut req = vec![0x01, sub];
        req.extend_from_slice(params);
        self.ch.send_report(RID, &req)
    }

    /// `[1,1,0]` — 設定をフラッシュへ保存 (refereshToFlash)。
    /// 実機版は「最後の変更から ~1 秒後に 1 回」へ合体する (公式 saveEqInfo の
    /// 1 秒待ちと同じ + スライダードラッグ中の連続 apply でフラッシュを
    /// 擦り減らさないため)。世代カウンタ方式で取りこぼし無し。
    #[cfg(not(test))]
    fn save(&self) -> HubResult<()> {
        // fail-fast 中は予約だけしない (書き込み自体も拒否されるので)
        if self.fails.load(Ordering::Relaxed) >= FAIL_FAST_AFTER {
            return Err(HubError::Disconnected);
        }
        self.save_gen.fetch_add(1, Ordering::Relaxed);
        if self
            .saving
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(()); // ワーカー稼働中 — 積んだ世代は後で拾われる
        }
        let ch = self.ch.clone();
        let gen = self.save_gen.clone();
        let saving = self.saving.clone();
        std::thread::spawn(move || {
            loop {
                let g = gen.load(Ordering::Acquire);
                std::thread::sleep(Duration::from_secs(1));
                if gen.load(Ordering::Acquire) != g {
                    continue; // まだ変更が来ている — 静まるまで待つ
                }
                let _ = ch.send_report(RID, &[0x01, 0x01, 0x00]);
                if gen.load(Ordering::Acquire) != g {
                    continue; // 送信中に積まれた分はもう一周
                }
                saving.store(false, Ordering::Release);
                // store(false) との競合: 直前に積まれた世代は自分で拾う
                // (相手は saving==true を見てワーカーを立てていない可能性がある)。
                if gen.load(Ordering::Acquire) == g || saving.swap(true, Ordering::AcqRel) {
                    break;
                }
            }
        });
        Ok(())
    }

    #[cfg(test)]
    fn save(&self) -> HubResult<()> {
        self.save_gen.fetch_add(1, Ordering::Relaxed);
        self.saving.store(false, Ordering::Relaxed);
        self.write(0x01, &[0])
    }

    /// biquad 係数 → 20B。公式の quantizer と同じ: [b0,b1,b2,-a1,-a2] を
    /// 固定小数で i32 LE に。HS (X1) のみ Q27+クランプ、それ以外 Q30。
    fn peq_coefs(b: &PeqBand) -> [i32; 5] {
        let c = devhub_core::peq::coefs(b.filter, b.freq as f32, b.q, b.gain, devhub_core::peq::FS);
        let mut out = [0i32; 5];
        if b.filter == PeqFilter::HighShelf {
            const SCALE: i64 = 1 << 27;
            const MAX: i64 = SCALE - 1;
            for (o, x) in out.iter_mut().zip(c) {
                *o = (x * SCALE as f64).round().clamp(-MAX as f64, MAX as f64) as i64 as i32;
            }
        } else {
            for (o, x) in out.iter_mut().zip(c) {
                // 公式は丸めた整数を int32 へそのまま流す (2^30 超は wrap)。
                *o = (x * (1i64 << 30) as f64).round() as i64 as i32;
            }
        }
        out
    }

    /// EQ バンド書き込みフレーム (36B) — 公式 buildEqBuffer と同一レイアウト。
    /// write() が先頭に [1,9] を付けるので、ここでは v[2..] を返す。
    fn peq_params(&self, band: usize, b: &PeqBand) -> Vec<u8> {
        let mut v = Vec::with_capacity(34);
        v.push(24); // 4 + 係数 20B
        v.extend_from_slice(&[0, band as u8, 0, 0]);
        for c in Self::peq_coefs(b) {
            v.extend_from_slice(&c.to_le_bytes());
        }
        v.extend_from_slice(&b.freq.to_le_bytes());
        v.extend_from_slice(&((b.q * 256.).round() as i32 as u16).to_le_bytes());
        v.extend_from_slice(&((b.gain * 256.).round() as i32 as i16).to_le_bytes());
        v.push(wire_filter(b.filter));
        v.push(0); // offset (公式 t.offset、常に 0 を観測)
        v.push(*self.eq_tag.lock().unwrap_or_else(|p| p.into_inner()));
        debug_assert_eq!(v.len(), 34);
        v
    }

    /// バンド 1 個分を書き込みフレームで送る。
    fn write_peq_band(&self, band: usize, b: &PeqBand) -> HubResult<()> {
        self.write(sub::EQ, &self.peq_params(band, b))
    }

    /// `[1,10,4,0,0,255,255]` — 書き込んだ EQ をレジスタへ適用 (refereshToMemery)。
    fn apply_peq(&self) -> HubResult<()> {
        self.write(sub::APPLY, &[4, 0, 0, 255, 255])
    }

    fn set(&self, id: &str, v: SettingValue, st: &mut DeviceState) {
        st.settings.insert(id.to_string(), v);
    }
}

impl DeviceDriver for WalkplayDriver {
    fn info(&self) -> DeviceInfo {
        self.info.clone()
    }

    fn caps(&self) -> Capabilities {
        if let Some(c) = self.caps.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return c;
        }
        let c = Capabilities {
            settings: vec![
                SettingSpec::choice(
                    "dac_filter",
                    section::AUDIO,
                    "デジタルフィルター",
                    FILTERS
                        .iter()
                        .enumerate()
                        .map(|(i, n)| ChoiceOpt::new(i as i64, *n))
                        .collect(),
                ),
                SettingSpec::choice(
                    "dac_work_mode",
                    section::AUDIO,
                    "DAC ワークモード",
                    vec![ChoiceOpt::new(0, "Class-H"), ChoiceOpt::new(1, "Class-AB")],
                ),
                SettingSpec::choice(
                    "output_gain",
                    section::AUDIO,
                    "出力ゲインモード",
                    vec![ChoiceOpt::new(0, "Low"), ChoiceOpt::new(1, "High")],
                ),
                SettingSpec::slider(
                    "channel_balance",
                    section::AUDIO,
                    "チャンネルバランス",
                    -127.,
                    127.,
                    1.,
                    "",
                )
                .help("正 = 右に寄せる (左を減衰)。0 で中央。"),
                SettingSpec::slider(
                    "eq_volume",
                    section::AUDIO,
                    "EQ ボリューム",
                    -16.,
                    6.,
                    1.,
                    "dB",
                ),
                SettingSpec::slider(
                    "mic_offset",
                    section::AUDIO,
                    "マイクオフセット",
                    -15.,
                    15.,
                    1.,
                    "",
                ),
            ],
            actions: vec![ActionSpec::new(
                "eq_def",
                section::AUDIO,
                "EQ を工場出荷値 (DEF) に戻す",
            )
            .help("公式 DEF プリセットの 8 バンドを書き込みます (バンド 9/10 は変わりません)")
            .confirm("EQ を DEF プリセットに戻します。よろしいですか?")],
            profiles: None,
            mouse: None,
            keyboard: None,
            lighting: vec![],
            battery: false,
            peq: Some(PeqCaps {
                bands: 10,
                freq_min: 20,
                freq_max: 20_000,
                q_min: 0.5,
                q_max: 10.,
                gain_min: -12.,
                gain_max: 12.,
                filters: vec![
                    PeqFilter::LowShelf,
                    PeqFilter::Peak,
                    PeqFilter::HighShelf,
                    PeqFilter::LowPass,
                    PeqFilter::HighPass,
                ],
            }),
            firmware_feed: None,
        };
        *self.caps.lock().unwrap_or_else(|p| p.into_inner()) = Some(c.clone());
        c
    }

    fn read(&self) -> HubResult<DeviceState> {
        let mut st = DeviceState {
            unsaved: false,
            profile: ProfileState {
                active: 0,
                names: vec![],
            },
            settings: Default::default(),
            mouse: None,
            keyboard: None,
            lighting: Default::default(),
            peq: None,
        };
        if let Ok(v) = self.query_u8(sub::FILTER_IDX, &[]) {
            self.set(
                "dac_filter",
                SettingValue::Choice((v as i64 - 1).clamp(0, 4)),
                &mut st,
            );
        }
        if let Ok(v) = self.query_u8(sub::WORK_MODE, &[]) {
            self.set(
                "dac_work_mode",
                SettingValue::Choice((v as i64).clamp(0, 1)),
                &mut st,
            );
        }
        if let Ok(v) = self.query_u8(sub::GAIN_MODE, &[]) {
            self.set(
                "output_gain",
                SettingValue::Choice((v as i64).clamp(0, 1)),
                &mut st,
            );
        }
        // バランス: 各 ch の減衰値 (e[5], 0 か負値エンコード) → 1 本スライダーに合成。
        // 公式応答ディスパッチャ: ch0 減衰 → +r、ch1 減衰 → -r。
        // 値 = ch0減衰 - ch1減衰 (正 = 左が減衰され音が右に寄る、公式と同じ向き)。
        let mut atten = [0f32; 2];
        for ch in 0..2u8 {
            if let Ok(r) = self.query(sub::BALANCE, &[2, ch, 0]) {
                let v = r.get(3).copied().unwrap_or(0);
                atten[ch as usize] = if v != 0 { 256. - v as f32 } else { 0. };
            }
        }
        self.set(
            "channel_balance",
            SettingValue::Num(atten[0] - atten[1]),
            &mut st,
        );
        // EQ ボリューム: e[4] = data[2] = raw → UI 値 = raw - 16 (範囲 -16..+6)
        if let Ok(r) = self.query(sub::EQ_VOLUME, &[2, 0, 0]) {
            let v = r.get(2).copied().unwrap_or(16);
            self.set("eq_volume", SettingValue::Num(v as f32 - 16.), &mut st);
        }
        // マイクオフセット: e[3..4] int16 LE → UI 値 = round(raw*15/32767)
        if let Ok(r) = self.query(sub::MIC_OFFSET, &[0]) {
            let lo = r.get(1).copied().unwrap_or(0) as i32;
            let hi = r.get(2).copied().unwrap_or(0) as i32;
            let raw = (lo | (hi << 8)) as i16 as i32;
            self.set(
                "mic_offset",
                SettingValue::Num((raw as f32 * 15. / 32767.).round()),
                &mut st,
            );
        }
        // DAC EQ: [80,9,0,0,band] でバンド別フレーム。
        // 応答 = [4B,80,09] + param エコー [0,0,band] + 書き込みフレーム v[5..35]
        // (実機ダンプで確認)。query() は b[3..] を返すので:
        // r[0..2]=param エコー, r[5..24]=係数, r[25..26]=freq u16LE,
        // r[27..28]=q*256, r[29..30]=gain*256 i16LE, r[31]=type,
        // r[32]=offset, r[33]=tag。
        let mut bands = vec![];
        let mut ok = false;
        for i in 0..10usize {
            match self.query(sub::EQ, &[0, 0, i as u8]) {
                Ok(r) if r.len() >= 34 => {
                    let freq = r[25] as u16 | (r[26] as u16) << 8;
                    let q = (r[27] as u16 | (r[28] as u16) << 8) as f32 / 256.;
                    let gain = i16::from_le_bytes([r[29], r[30]]) as f32 / 256.;
                    if i == 0 {
                        *self.eq_tag.lock().unwrap_or_else(|p| p.into_inner()) = r[33];
                    }
                    bands.push(PeqBand {
                        freq,
                        q: (q * 100.).round() / 100.,
                        gain,
                        filter: from_wire_filter(r[31]),
                    });
                    ok = true;
                }
                _ => break,
            }
        }
        if ok && bands.len() == 10 {
            st.peq = Some(bands);
        }
        Ok(st)
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::Setting { id, value } => match id.as_str() {
                "dac_filter" => {
                    let v = value.as_choice().clamp(0, 4) as u8;
                    self.write(0x11, &[1, v + 1])?;
                }
                "dac_work_mode" => {
                    let v = value.as_choice().clamp(0, 1) as u8;
                    self.write(0x1D, &[1, v])?;
                }
                "output_gain" => {
                    let v = value.as_choice().clamp(0, 1) as u8;
                    self.write(0x19, &[1, v])?;
                }
                "channel_balance" => {
                    let v = value.as_num().clamp(-127., 127.);
                    // 公式 setDacBalance: 減衰させる側を書き、反対側を 0 クリアする
                    // (書かなかった側の古い減衰値は残る)。
                    // 正 = ch0 減衰 (公式の t>0 側 = 音が右へ)。負 = ch1 減衰。
                    if v > 0. {
                        self.write(0x16, &[4, 0, 0, (-(v as i8)) as u8, 0])?;
                        self.write(0x16, &[4, 1, 0, 0, 0])?;
                    } else if v < 0. {
                        self.write(0x16, &[4, 0, 0, 0, 0])?;
                        self.write(0x16, &[4, 1, 0, (v as i8) as u8, 0])?;
                    } else {
                        self.write(0x16, &[4, 0, 0, 0, 0])?;
                        self.write(0x16, &[4, 1, 0, 0, 0])?;
                    }
                }
                "eq_volume" => {
                    let v = value.as_num().clamp(-16., 6.);
                    self.write(0x03, &[2, 0, (v + 16.) as u8])?;
                }
                "mic_offset" => {
                    // 公式 setMicphoneOffset: UI -15..15 → int16 (v*32767/15) LE
                    let v = value.as_num().clamp(-15., 15.);
                    let raw = (v * 32767. / 15.).round().clamp(-32767., 32767.) as i16 as u16;
                    self.write(0x02, &[2, (raw & 0xFF) as u8, (raw >> 8) as u8])?;
                }
                _ => return Err(HubError::Invalid(format!("未知の設定 {id}"))),
            },
            Change::Peq { band, value } => {
                if *band >= 10 {
                    return Err(HubError::Invalid(format!("バンド {band} は範囲外")));
                }
                self.write_peq_band(*band, value)?;
                self.apply_peq()?;
            }
            Change::PeqAll { bands } => {
                if bands.len() > 10 {
                    return Err(HubError::Invalid("バンド数が多すぎます".into()));
                }
                for (i, b) in bands.iter().enumerate() {
                    self.write_peq_band(i, b)?;
                }
                self.apply_peq()?;
            }
            Change::Profile(_) => return Err(HubError::Unsupported("profiles")),
            _ => return Err(HubError::Unsupported("この変更")),
        }
        // 全書き込みの後にフラッシュへ保存 (電源を落としても残る)
        self.save()
    }

    fn run_action(&self, id: &str, _file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            // 公式バンドルにハードコードされている DEF プリセット 8 バンド分
            // (50/200/500/1000/2500/5000/10000/15000 Hz, PK, Q 0.75, 0dB)。
            // 公式はバンド 8/9 に触らないので、こちらも 8 本だけ書く。
            "eq_def" => {
                const DEF_FREQS: [u16; 8] = [50, 200, 500, 1000, 2500, 5000, 10000, 15000];
                for (i, f) in DEF_FREQS.iter().enumerate() {
                    self.write_peq_band(
                        i,
                        &PeqBand {
                            freq: *f,
                            q: 0.75,
                            gain: 0.,
                            filter: PeqFilter::Peak,
                        },
                    )?;
                }
                self.apply_peq()?;
                self.save()?;
                Ok(ActionOutcome {
                    message: Some("DEF プリセットを適用しました".into()),
                    reload: true,
                })
            }
            _ => Err(HubError::Unsupported("action")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devhub_core::model::Change;

    fn mock_driver() -> WalkplayDriver {
        WalkplayDriver {
            ch: Arc::new(Channel::mock()),
            caps: Mutex::new(None),
            fails: AtomicU32::new(0),
            dead_since: Mutex::new(None),
            eq_tag: Mutex::new(0),
            save_gen: Arc::new(AtomicU64::new(0)),
            saving: Arc::new(AtomicBool::new(false)),
            info: DeviceInfo {
                id: DeviceId("t".into()),
                kind: DeviceKind::Other,
                name: "TP55".into(),
                vendor: "WalkPlay".into(),
                model: "tp55".into(),
                vid: 0x3302,
                pid: 0x4383,
                transport: Transport::Wired,
                firmware: Some("1.4".into()),
                dongle_firmware: None,
                serial: None,
                driver: DriverKind::Walkplay,
                battery: None,
                supported: true,
                asleep: false,
                board_id: None,
            },
        }
    }

    /// 応答フレーム: report 0x4B + [0x80,sub] エコー + データ。
    fn reply(sub: u8, data: &[u8]) -> Vec<u8> {
        let mut r = vec![0x4B, 0x80, sub];
        r.extend_from_slice(data);
        r.resize(65, 0);
        r
    }

    #[test]
    fn read_settings_fixture() {
        let d = mock_driver();
        // dac_filter: e[3]=3 → index 2 (SLOW-LL)
        d.ch.mock_reply(&reply(0x11, &[0, 3]));
        // work_mode: e[3]=0 → Class-H
        d.ch.mock_reply(&reply(0x1D, &[0, 0]));
        // output_gain: e[3]=1 → High
        d.ch.mock_reply(&reply(0x19, &[0, 1]));
        // balance L/R: [80,16,2,ch,0] → e[3]=ch, e[5]=val。
        // ch0 (L) が -30 → 値 = +30 (公式: ch0減衰=正)
        d.ch.mock_reply(&reply(0x16, &[2, 0, 0, 0xE2]));
        d.ch.mock_reply(&reply(0x16, &[2, 1, 0, 0]));
        // eq_volume: e[4]=15 → UI 値 -1 dB
        d.ch.mock_reply(&reply(0x03, &[2, 0, 15]));
        // mic_offset: e[3..4] int16 LE = 10922 → round(10922*15/32767) = +5
        d.ch.mock_reply(&reply(0x02, &[0, 0xAA, 0x2A]));

        let st = d.read().unwrap();
        assert_eq!(
            st.settings["dac_filter"],
            SettingValue::Choice(2),
            "filter index"
        );
        assert_eq!(st.settings["dac_work_mode"], SettingValue::Choice(0));
        assert_eq!(st.settings["output_gain"], SettingValue::Choice(1));
        assert_eq!(st.settings["channel_balance"], SettingValue::Num(30.0));
        assert_eq!(st.settings["eq_volume"], SettingValue::Num(-1.0));
        assert_eq!(st.settings["mic_offset"], SettingValue::Num(5.0));
    }

    #[test]
    fn write_wire_fixture() {
        let d = mock_driver();
        // 書き込み系は応答を待たない — 送信列だけ検証
        d.apply(&Change::Setting {
            id: "dac_filter".into(),
            value: SettingValue::Choice(4),
        })
        .unwrap();
        d.apply(&Change::Setting {
            id: "output_gain".into(),
            value: SettingValue::Choice(0),
        })
        .unwrap();
        d.apply(&Change::Setting {
            id: "channel_balance".into(),
            value: SettingValue::Num(20.0),
        })
        .unwrap();
        d.apply(&Change::Setting {
            id: "mic_offset".into(),
            value: SettingValue::Num(5.0),
        })
        .unwrap();
        d.apply(&Change::Setting {
            id: "eq_volume".into(),
            value: SettingValue::Num(-3.0),
        })
        .unwrap();
        let sent = d.ch.mock_sent();
        assert_eq!(
            sent,
            vec![
                // [0x4B, 1, 17, 1, 5] フィルター idx+1=5 (NON-OS)
                vec![0x4B, 1, 17, 1, 5],
                vec![0x4B, 1, 1, 0],     // save
                vec![0x4B, 1, 25, 1, 0], // gain Low
                vec![0x4B, 1, 1, 0],
                // balance +20 → ch0 減衰 -20 + ch1 クリア (公式と同じ 2 フレーム)
                vec![0x4B, 1, 22, 4, 0, 0, 236, 0],
                vec![0x4B, 1, 22, 4, 1, 0, 0, 0],
                vec![0x4B, 1, 1, 0],
                // mic_offset +5 → int16 10922 = 0x2AAA → [lo,hi]
                vec![0x4B, 1, 2, 2, 0xAA, 0x2A],
                vec![0x4B, 1, 1, 0],
                // eq_volume -3 → raw 13
                vec![0x4B, 1, 3, 2, 0, 13],
                vec![0x4B, 1, 1, 0],
            ]
        );
    }

    /// EQ 応答の実機レイアウト: [4B,80,09] + param エコー [0,0,band]
    /// + v[5..] = [0,0] + 係数 20B + freq + q + gain + type + offset + tag。
    fn eq_reply(band: u8, freq: u16, q256: u16, gain256: i16, ty: u8, tag: u8) -> Vec<u8> {
        let mut f = vec![0x4B, 0x80, 0x09, 0, 0, band, 0, 0];
        f.extend_from_slice(&[0u8; 20]); // 係数領域 (デコードしない)
        f.extend_from_slice(&freq.to_le_bytes());
        f.extend_from_slice(&q256.to_le_bytes());
        f.extend_from_slice(&gain256.to_le_bytes());
        f.extend_from_slice(&[ty, 0, tag]);
        f.resize(65, 0);
        f
    }

    #[test]
    fn peq_coefs_match_device() {
        // 実機 band0 (freq=31, q=0.75, gain=+2, PK) の読み取り係数と
        // バイト単位で一致すること (96kHz・Q30 で検証済み)。
        let c = WalkplayDriver::peq_coefs(&PeqBand {
            freq: 31,
            q: 0.75,
            gain: 2.,
            filter: PeqFilter::Peak,
        });
        assert_eq!(
            c,
            [
                0x40051BA5u32 as i32,
                -0x7FD87A32,
                0x3FD36FCB,
                0x7FD87A32,
                -0x3FD88B71
            ]
        );
    }

    #[test]
    fn peq_read_decode_fixture() {
        let d = mock_driver();
        // スカラー設定は全てタイムアウトさせて失敗させないため、まず通常分を埋める
        d.ch.mock_reply(&reply(0x11, &[0, 3]));
        d.ch.mock_reply(&reply(0x1D, &[0, 0]));
        d.ch.mock_reply(&reply(0x19, &[0, 1]));
        d.ch.mock_reply(&reply(0x16, &[2, 0, 0, 0]));
        d.ch.mock_reply(&reply(0x16, &[2, 1, 0, 0]));
        d.ch.mock_reply(&reply(0x03, &[2, 0, 15]));
        d.ch.mock_reply(&reply(0x02, &[0, 0, 0]));
        // EQ 10 バンド (band1 = 62Hz 実機値、tag=0x65)
        let freqs = [31u16, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
        for (i, f) in freqs.iter().enumerate() {
            d.ch.mock_reply(&eq_reply(
                i as u8,
                *f,
                (0.75 * 256.) as u16,
                if i == 0 { 512 } else { 0 }, // band0 gain +2
                2,
                0x65,
            ));
        }
        let st = d.read().unwrap();
        let bands = st.peq.expect("peq bands");
        assert_eq!(bands.len(), 10);
        assert_eq!(bands[0].freq, 31);
        assert_eq!(bands[1].freq, 62);
        assert_eq!(bands[0].gain, 2.);
        assert_eq!(bands[0].filter, PeqFilter::Peak);
        assert_eq!(*d.eq_tag.lock().unwrap(), 0x65);
    }

    #[test]
    fn peq_write_wire_fixture() {
        let d = mock_driver();
        *d.eq_tag.lock().unwrap() = 0x65;
        d.apply(&Change::Peq {
            band: 0,
            value: PeqBand {
                freq: 31,
                q: 0.75,
                gain: 2.,
                filter: PeqFilter::Peak,
            },
        })
        .unwrap();
        let sent = d.ch.mock_sent();
        // [0x4B, 1, 9, 24, 0, 0, 0,0, <20B coefs>, 1F,00, C0,00, 00,02, 2, 0, 0x65]
        let mut expect = vec![0x4B, 1, 9, 24, 0, 0, 0, 0];
        for c in [
            0x40051BA5u32 as i32,
            -0x7FD87A32,
            0x3FD36FCB,
            0x7FD87A32,
            -0x3FD88B71,
        ] {
            expect.extend_from_slice(&c.to_le_bytes());
        }
        expect.extend_from_slice(&[0x1F, 0, 0xC0, 0, 0, 2, 2, 0, 0x65]);
        assert_eq!(sent[0], expect, "36B band frame");
        assert_eq!(sent[1], vec![0x4B, 1, 10, 4, 0, 0, 255, 255], "apply");
        assert_eq!(sent[2], vec![0x4B, 1, 1, 0], "save");
        assert_eq!(sent.len(), 3);
    }

    #[test]
    fn peq_all_wire_fixture() {
        let d = mock_driver();
        let bands = vec![
            PeqBand {
                freq: 100,
                q: 1.,
                gain: 0.,
                filter: PeqFilter::LowPass,
            };
            10
        ];
        d.apply(&Change::PeqAll { bands }).unwrap();
        let sent = d.ch.mock_sent();
        assert_eq!(sent.len(), 12, "10 frames + apply + save");
        assert_eq!(sent[10], vec![0x4B, 1, 10, 4, 0, 0, 255, 255]);
        assert_eq!(sent[11], vec![0x4B, 1, 1, 0]);
        // band 9 フレームのバンド番号位置
        assert_eq!(sent[9][5], 9);
    }
}
