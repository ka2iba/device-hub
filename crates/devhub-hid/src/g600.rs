//! Logitech G600 のオンボード設定。SET は退避と保存に必要な 2 種類に限定する。

use crate::{transport::Channel, Watcher};
use devhub_core::backend::*;
use devhub_core::model::*;
use std::path::Path;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

const STATUS: u8 = 0xF0;
const LED_STATE: u8 = 0xF1;
/// 4 バイト目が「今効いている DPI (×50)」(2026-11-19 実機観察、§3.9)。
const DPI_STATE: u8 = 0xF2;
const FIRST_PROFILE: u8 = 0xF3;
const PROFILE_COUNT: u8 = 3;
const PROFILE_LEN: usize = 154;
const NORMAL_BUTTONS: usize = 31;
const SHIFT_BUTTONS: usize = 94;
const BUTTON_COUNT: usize = 20;
const DPI_SLOTS: std::ops::Range<usize> = 14..18;
const POLLING: [(i64, u8); 4] = [(125, 7), (250, 3), (500, 1), (1000, 0)];
const MOUSE_BUTTONS: [MouseFn; 5] = [
    MouseFn::Left,
    MouseFn::Right,
    MouseFn::Middle,
    MouseFn::Back,
    MouseFn::Forward,
];

type Report = [u8; PROFILE_LEN];

pub struct G600 {
    ch: Channel,
    info: DeviceInfo,
    events: Option<Watcher>,
    opened_at: Instant,
    state: Mutex<State>,
}

struct State {
    device: [Report; PROFILE_COUNT as usize],
    pending: Option<(u8, Report)>,
}

/// SET の呼び出し元が任意のコマンドを送れないようにする。
enum Write<'a> {
    Select(u8),
    Profile(u8, &'a Report),
    /// 現在の DPI 段の再設定 `[F0, 40|(r<<1), 0, 0]`。保存済みの DPI を即時に反映させる (§3.9 E5)。
    /// `profile` は段 `r` を検証したプロファイル。送信直前の使用中プロファイルが違えば送らない
    /// (本体のボタンでプロファイルが変わった場合に、別プロファイルの無効段を選ばないため)。
    Resolution {
        profile: u8,
        r: u8,
    },
}

pub fn open(dev: hidapi::HidDevice, info: DeviceInfo, events: Option<Watcher>) -> HubResult<G600> {
    G600::new(Channel::open(dev, 256, PROFILE_LEN), info, events)
}

fn protocol(message: impl Into<String>) -> HubError {
    HubError::Protocol(message.into())
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.into())
}

fn decode_status(b: u8) -> HubResult<(u8, u8, bool)> {
    let profile = b >> 4;
    if profile >= PROFILE_COUNT {
        return Err(protocol(format!(
            "G600: プロファイル番号 {profile} が不正です"
        )));
    }
    Ok((profile, (b >> 1) & 3, b & 8 != 0))
}

fn status(ch: &Channel) -> HubResult<(u8, u8, bool)> {
    decode_status(ch.get_feature_id(STATUS, 4)?[1])
}

fn read_profile(ch: &Channel, profile: u8) -> HubResult<Report> {
    ch.get_feature_id(FIRST_PROFILE + profile, PROFILE_LEN)?
        .try_into()
        .map_err(|_| protocol("G600: プロファイルの長さが不正です"))
}

fn read_profiles(ch: &Channel) -> HubResult<[Report; PROFILE_COUNT as usize]> {
    status(ch)?;
    Ok([
        read_profile(ch, 0)?,
        read_profile(ch, 1)?,
        read_profile(ch, 2)?,
    ])
}

fn button_offset(slot: usize) -> HubResult<usize> {
    match slot {
        0..20 => Ok(NORMAL_BUTTONS + slot * 3),
        20..40 => Ok(SHIFT_BUTTONS + (slot - BUTTON_COUNT) * 3),
        _ => Err(invalid("G600: ボタン番号が範囲外です")),
    }
}

fn decode_button(bytes: &[u8], shift: u8) -> ButtonAction {
    let (code, mods, key) = (bytes[0], bytes[1], bytes[2]);
    match code {
        0 if mods == 0 && key == 0 => ButtonAction::Disabled,
        0 => ButtonAction::Key(KeyStroke {
            mods,
            usage: key.into(),
        }),
        1..=5 => ButtonAction::Mouse(MOUSE_BUTTONS[usize::from(code - 1)]),
        0x11 => ButtonAction::Dpi(DpiFn::Up),
        0x12 => ButtonAction::Dpi(DpiFn::Down),
        0x13 => ButtonAction::Dpi(DpiFn::Cycle),
        0x14 => ButtonAction::ProfileCycle,
        0x15 => ButtonAction::Dpi(DpiFn::Shift(if shift == 0 {
            200
        } else {
            u32::from(shift) * 50
        })),
        0x17 => ButtonAction::GShift,
        _ => ButtonAction::Unsupported {
            ty: code,
            params: vec![mods, key],
        },
    }
}

fn encode_button(action: &ButtonAction) -> HubResult<[u8; 3]> {
    let code = match action {
        ButtonAction::Disabled => 0,
        ButtonAction::Key(key) => {
            let usage =
                u8::try_from(key.usage).map_err(|_| invalid("G600: キー usage が範囲外です"))?;
            return Ok([0, key.mods, usage]);
        }
        ButtonAction::Mouse(button) => {
            let index = MOUSE_BUTTONS
                .iter()
                .position(|m| m == button)
                .ok_or_else(|| invalid("G600: 対応していないマウス操作です"))?;
            index as u8 + 1
        }
        ButtonAction::Dpi(DpiFn::Up) => 0x11,
        ButtonAction::Dpi(DpiFn::Down) => 0x12,
        ButtonAction::Dpi(DpiFn::Cycle) => 0x13,
        ButtonAction::ProfileCycle => 0x14,
        ButtonAction::Dpi(DpiFn::Shift(_)) => 0x15,
        ButtonAction::GShift => 0x17,
        ButtonAction::Unsupported { ty, params } => match params.as_slice() {
            [mods, key] => return Ok([*ty, *mods, *key]),
            _ => {
                return Err(invalid(
                    "G600: 未対応割り当てのパラメータは 2 バイト必要です",
                ))
            }
        },
        _ => return Err(HubError::Unsupported("G600 button action")),
    };
    Ok([code, 0, 0])
}

fn has_left_click(data: &Report) -> bool {
    data[NORMAL_BUTTONS..NORMAL_BUTTONS + BUTTON_COUNT * 3]
        .chunks_exact(3)
        .any(|button| button[0] == 1)
}

fn usable(data: &Report) -> bool {
    has_left_click(data) && data[DPI_SLOTS].iter().any(|&dpi| dpi != 0)
}

fn light(color: &[u8], effect: u8, speed: u8) -> LightingState {
    LightingState {
        on: true,
        effect,
        color: Rgb::new(color[0], color[1], color[2]),
        brightness: 0,
        speed,
        direction: 0,
        per_key: Default::default(),
        zone_on: vec![],
        palette: vec![],
        palette_sel: 0,
        color_correction: None,
    }
}

fn decode_profile(p: u8, data: &Report, unsaved: bool) -> DeviceState {
    let mut active_stage = 0;
    let mut stages = Vec::new();
    for (slot, &units) in data[DPI_SLOTS].iter().enumerate() {
        if units != 0 {
            if slot + 1 == usize::from(data[13]) {
                active_stage = stages.len();
            }
            stages.push(DpiStage {
                x: u32::from(units) * 50,
                y: None,
                color: Rgb::default(),
                enabled: true,
            });
        }
    }
    let buttons = [NORMAL_BUTTONS, SHIFT_BUTTONS]
        .into_iter()
        .flat_map(|start| {
            data[start..start + BUTTON_COUNT * 3]
                .chunks_exact(3)
                .map(|bytes| decode_button(bytes, data[12]))
        })
        .collect();
    DeviceState {
        profile: ProfileState {
            active: p,
            names: vec![String::new(); PROFILE_COUNT as usize],
        },
        settings: POLLING
            .iter()
            .find(|&&(_, code)| code == data[11])
            .map(|&(hz, _)| ("polling".into(), SettingValue::Choice(hz)))
            .into_iter()
            .collect(),
        mouse: Some(MouseState {
            stages,
            active_stage,
            buttons,
            macros: vec![],
            adaptive: None,
        }),
        keyboard: None,
        lighting: [
            (0, light(&data[1..4], data[4], 15 - data[5].clamp(1, 15))),
            (1, light(&data[91..94], 0, 0)),
        ]
        .into(),
        peq: None,
        unsaved,
    }
}

fn update(data: &mut Report, change: &Change) -> HubResult<bool> {
    let mut shared_shift_changed = false;
    match change {
        Change::Setting { id, value } if id == "polling" => {
            let SettingValue::Choice(hz) = value else {
                return Err(invalid("G600: ポーリングレートが不正です"));
            };
            data[11] = POLLING
                .iter()
                .find(|&&(rate, _)| rate == *hz)
                .ok_or_else(|| invalid("G600: ポーリングレートが範囲外です"))?
                .1;
        }
        Change::DpiStages { stages, active } => {
            if stages.is_empty() || stages.len() > 4 || *active >= stages.len() {
                return Err(invalid("G600: DPI ステージ数または既定段が範囲外です"));
            }
            data[DPI_SLOTS].fill(0);
            for (units, stage) in data[DPI_SLOTS].iter_mut().zip(stages) {
                *units = ((stage.x.clamp(200, 8200) + 25) / 50) as u8;
            }
            data[13] = *active as u8 + 1;
        }
        Change::ActiveStage(index) => {
            let (slot, _) = data[DPI_SLOTS]
                .iter()
                .enumerate()
                .filter(|&(_, &units)| units != 0)
                .nth(*index)
                .ok_or_else(|| invalid("G600: DPI ステージが範囲外です"))?;
            data[13] = slot as u8 + 1;
        }
        Change::Button { slot, action } => {
            let offset = button_offset(*slot)?;
            let encoded = encode_button(action)?;
            let previous = decode_button(&data[offset..offset + 3], data[12]);
            if previous != *action {
                data[offset..offset + 3].copy_from_slice(&encoded);
            }
            if !has_left_click(data) {
                return Err(invalid(
                    "G-Shift でない側 (G1–G20) に左クリックが 1 つ必要です",
                ));
            }
            if let ButtonAction::Dpi(DpiFn::Shift(dpi)) = action {
                let units = u8::try_from(dpi / 50)
                    .map_err(|_| invalid("G600: DPI シフト値が範囲外です"))?;
                shared_shift_changed = units != data[12]
                    && [NORMAL_BUTTONS, SHIFT_BUTTONS].into_iter().any(|start| {
                        (start..start + BUTTON_COUNT * 3)
                            .step_by(3)
                            .any(|other| other != offset && data[other] == 0x15)
                    });
                data[12] = units;
            }
        }
        Change::Lighting { area: 0, state } => {
            if state.effect > 2 {
                return Err(invalid("G600: LED エフェクトが範囲外です"));
            }
            data[1..4].copy_from_slice(&[state.color.r, state.color.g, state.color.b]);
            data[4] = state.effect;
            // 固定 (0) は周期を持たない。実機の固定プロファイルは周期 0 なので、
            // 速度スライダーの値で書き換えず元のバイトを残す (保存後の 0xF1 照合も周期を含むため)。
            if state.effect != 0 {
                data[5] = 15 - state.speed.min(14);
            }
        }
        Change::Lighting { area: 1, state } => {
            data[91..94].copy_from_slice(&[state.color.r, state.color.g, state.color.b]);
        }
        _ => return Err(HubError::Unsupported("G600 change")),
    }
    Ok(shared_shift_changed)
}

impl G600 {
    fn new(ch: Channel, mut info: DeviceInfo, events: Option<Watcher>) -> HubResult<Self> {
        let opened_at = Instant::now();
        let device = read_profiles(&ch)?;
        info.firmware = None;
        info.dongle_firmware = None;
        info.battery = None;
        Ok(Self {
            ch,
            info,
            events,
            opened_at,
            state: Mutex::new(State {
                device,
                pending: None,
            }),
        })
    }

    fn emit(&self, event: HubEvent) {
        if let Some(events) = &self.events {
            events(event);
        }
    }

    fn progress(&self, done: bool) {
        self.emit(HubEvent::Progress {
            id: self.info.id.clone(),
            label: "保存中… マウスに触らないでください".into(),
            fraction: None,
            done,
        });
    }

    fn wait_ready(&self) -> HubResult<u8> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let (active, _, ready) = status(&self.ch)?;
            if ready && Instant::now() <= deadline {
                return Ok(active);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(protocol("G600: 保存処理の待機がタイムアウトしました"));
            }
            sleep(remaining.min(Duration::from_millis(100)));
        }
    }

    /// 呼び出し元は state のロックを保持する。エラーを返した SET でも保存が
    /// 始まった可能性があるため、送信結果にかかわらず待機する。
    fn guarded_set(&self, write: Write<'_>) -> HubResult<u8> {
        sleep(Duration::from_secs(2).saturating_sub(self.opened_at.elapsed()));
        let active = self.wait_ready()?;
        let select;
        let data = match write {
            Write::Select(p) if p < PROFILE_COUNT => {
                select = [STATUS, 0x80 | (p << 4), 0, 0];
                select.as_slice()
            }
            Write::Resolution { profile, r } if usize::from(r) < DPI_SLOTS.len() => {
                if profile != active {
                    return Err(protocol(
                        "G600: プロファイルが切り替わったため DPI の再設定を中止しました",
                    ));
                }
                select = [STATUS, 0x40 | (r << 1), 0, 0];
                select.as_slice()
            }
            Write::Profile(p, report) if p < PROFILE_COUNT && report[0] == FIRST_PROFILE + p => {
                if p == active {
                    return Err(protocol(
                        "G600: 使用中のプロファイルへの書き込みを中止しました",
                    ));
                }
                report.as_slice()
            }
            _ => {
                return Err(invalid(
                    "G600: プロファイル番号またはレポート ID が不正です",
                ))
            }
        };
        let sent = self.ch.send_feature_id(data);
        sleep(Duration::from_millis(300));
        let ready = self.wait_ready();
        sent?;
        ready
    }

    fn select(&self, p: u8) -> HubResult<()> {
        if p >= PROFILE_COUNT {
            return Err(invalid("G600: プロファイル番号が範囲外です"));
        }
        if self.guarded_set(Write::Select(p))? != p {
            return Err(protocol(
                "G600: プロファイルの切り替えを確認できませんでした",
            ));
        }
        Ok(())
    }

    /// アクティブなプロファイル `data` の既定段で解像度を再設定し、保存済みの DPI を即時に反映させる。
    /// 既定段が無効 (範囲外・DPI 0) なら何も送らない。反映後は 0xF2 で効いている DPI を照合する。
    fn apply_resolution(&self, data: &Report) -> HubResult<()> {
        let profile = data[0].wrapping_sub(FIRST_PROFILE);
        if profile >= PROFILE_COUNT {
            return Err(protocol("G600: プロファイルのレポート ID が不正です"));
        }
        let Some(r) = data[13]
            .checked_sub(1)
            .filter(|&r| usize::from(r) < DPI_SLOTS.len())
        else {
            return Err(protocol("G600: 既定の DPI 段が不正です"));
        };
        let units = data[DPI_SLOTS.start + usize::from(r)];
        if units == 0 {
            return Err(protocol("G600: 既定の DPI 段が無効です"));
        }
        self.guarded_set(Write::Resolution { profile, r })?;
        if self.ch.get_feature_id(DPI_STATE, 5)?[3] != units {
            return Err(protocol("G600: DPI の反映を確認できませんでした"));
        }
        Ok(())
    }

    /// 保存に成功したら `Ok(None)`、保存はできたが DPI の即時反映を確認できなかったら `Ok(Some(注意文))`。
    fn save(&self, state: &mut State, p: u8, data: Report) -> HubResult<Option<String>> {
        let (active, _, _) = status(&self.ch)?;
        if !usable(&data) {
            return Err(invalid(
                "G600: 保存には通常側の左クリックと有効な DPI ステージが必要です",
            ));
        }
        let mut fallback = None;
        if p == active {
            for q in (0..PROFILE_COUNT).filter(|&q| q != p) {
                // 未検証のキャッシュを頼りに退避先を有効化しない。
                state.device[usize::from(q)] = read_profile(&self.ch, q)?;
                if usable(&state.device[usize::from(q)]) {
                    fallback = Some(q);
                    break;
                }
            }
            let q = fallback.ok_or_else(|| protocol("退避できるプロファイルがありません"))?;
            self.select(q)?;
        }

        for attempt in 0..2 {
            self.guarded_set(Write::Profile(p, &data))?;
            if read_profile(&self.ch, p)? == data {
                break;
            }
            if attempt == 1 {
                let location = fallback.map_or(String::new(), |q| {
                    format!("プロファイル {} に退避しています", q + 1)
                });
                return Err(protocol(format!("保存に失敗しました。{location}")));
            }
        }

        if let Some(q) = fallback {
            let verify = self.select(p).and_then(|()| {
                let led = self.ch.get_feature_id(LED_STATE, 8)?;
                if led[4..6] != data[4..6] {
                    return Err(protocol("G600: LED エフェクトまたは周期が一致しません"));
                }
                if read_profile(&self.ch, p)? != data {
                    return Err(protocol(
                        "G600: 選択後のプロファイルが保存内容と一致しません",
                    ));
                }
                Ok(())
            });
            if let Err(error) = verify {
                match self.select(q) {
                    Ok(()) => return Err(protocol(format!(
                        "保存内容の確認に失敗しました。プロファイル {} に退避しています。USB を挿し直して確認してください ({error})", q + 1
                    ))),
                    Err(HubError::Disconnected) => return Err(HubError::Disconnected),
                    Err(recovery) => return Err(protocol(format!(
                        "保存内容の確認に失敗しました ({error})。プロファイル {} への退避も確認できませんでした ({recovery})", q + 1
                    ))),
                }
            }
        }
        state.device[usize::from(p)] = data;
        state.pending = None;
        // 使用中のプロファイルを保存した場合だけ、DPI を即時に反映させる (失敗しても保存は成功)。
        if fallback.is_some() {
            if let Err(error) = self.apply_resolution(&data) {
                return Ok(Some(format!(
                    "DPI の変更は USB を挿し直すと反映されます ({error})"
                )));
            }
        }
        Ok(None)
    }
}

impl DeviceDriver for G600 {
    fn info(&self) -> DeviceInfo {
        self.info.clone()
    }

    fn caps(&self) -> Capabilities {
        let names = [
            "G1 左クリック",
            "G2 右クリック",
            "G3 ホイールクリック",
            "G4 ホイール左",
            "G5 ホイール右",
            "G6",
            "G7",
            "G8",
        ];
        let buttons = (0..BUTTON_COUNT)
            .cycle()
            .take(BUTTON_COUNT * 2)
            .map(|slot| ButtonSlot {
                name: names
                    .get(slot)
                    .map_or_else(|| format!("G{}", slot + 1), |&name| name.into()),
            })
            .collect();
        let area = |id, name: &str, effects, speed_max| LightArea {
            id,
            name: name.into(),
            effects,
            speed_max,
            brightness_max: 0,
            directions: vec![],
            switchable: false,
            zones: vec![],
            per_key: false,
            direct_drive: false,
            live_readback: false,
        };
        Capabilities {
            profiles: Some(ProfileCaps { count: PROFILE_COUNT, renamable: false }),
            settings: vec![SettingSpec::choice("polling", section::PERFORMANCE, "ポーリングレート",
                POLLING.iter().map(|&(hz, _)| ChoiceOpt::new(hz, format!("{hz} Hz"))).collect())],
            actions: vec![
                ActionSpec::new("save_onboard", section::SYSTEM, "本体に保存").commit()
                    .help("未保存の変更を G600 に書き込みます。保存中 (数秒) はマウスに触らないでください。"),
                ActionSpec::new("discard", section::SYSTEM, "変更を破棄")
                    .help("未保存の変更を捨てて、本体の内容を読み直します。"),
            ],
            mouse: Some(MouseCaps {
                figure: MouseFigure::Mmo12,
                layers: vec![
                    ButtonLayer { name: "通常".into(), help: None },
                    ButtonLayer {
                        name: "G-Shift".into(),
                        help: Some("G-Shift を割り当てたボタン (工場出荷時は G6) を押している間だけ有効な割り当てです。押している間は LED が「G-Shift 中」の色になります。".into()),
                    },
                ],
                dpi_min: 200, dpi_max: 8200, dpi_step: 50, min_stages: 1, max_stages: 4,
                xy_split: false, stage_colors: false, stage_toggle: false, buttons,
                button_kinds: vec![ButtonActionKind::Disabled, ButtonActionKind::Mouse, ButtonActionKind::Dpi,
                    ButtonActionKind::Key, ButtonActionKind::ProfileCycle, ButtonActionKind::GShift],
                require_left_click: true, macros: None, live_buttons: vec![], adaptive_dpi: None,
            }),
            lighting: vec![
                area(0, "本体", vec![LightEffect::new(0, "固定").color().still(),
                    LightEffect::new(1, "ブリージング").color(), LightEffect::new(2, "カラーサイクル")], 14),
                area(1, "G-Shift 中", vec![LightEffect::new(0, "固定").color().still()], 0),
            ],
            ..Capabilities::empty()
        }
    }

    fn read(&self) -> HubResult<DeviceState> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let (active, _, _) = status(&self.ch)?;
        if let Some((p, data)) = &state.pending {
            return Ok(decode_profile(*p, data, true));
        }
        let data = read_profile(&self.ch, active)?;
        state.device[usize::from(active)] = data;
        Ok(decode_profile(active, &data, false))
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let was_unsaved = state.pending.is_some();
        if let Change::Profile(p) = change {
            if was_unsaved {
                return Err(invalid(
                    "未保存の変更があります。保存するか破棄してから切り替えてください",
                ));
            }
            if *p >= PROFILE_COUNT {
                return Err(invalid("G600: プロファイル番号が範囲外です"));
            }
            let target = read_profile(&self.ch, *p)?;
            if !usable(&target) {
                return Err(invalid("G600: 通常側の左クリックと有効な DPI ステージがあるプロファイルを選んでください"));
            }
            self.select(*p)?;
            // プロファイル選択だけでは DPI が反映されないので、既定段で解像度を再設定する (libratbag と同じ)。
            return self.apply_resolution(&target);
        }
        let (p, mut data) = match state.pending {
            Some(pending) => pending,
            None => {
                let (active, _, _) = status(&self.ch)?;
                let data = read_profile(&self.ch, active)?;
                state.device[usize::from(active)] = data;
                (active, data)
            }
        };
        let shared_shift_changed = update(&mut data, change)?;
        let unsaved = data != state.device[usize::from(p)];
        state.pending = unsaved.then_some((p, data));
        drop(state);
        if unsaved != was_unsaved || shared_shift_changed {
            self.emit(HubEvent::StateChanged(self.info.id.clone()));
        }
        Ok(())
    }

    fn run_action(&self, id: &str, _file: Option<&Path>) -> HubResult<ActionOutcome> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match id {
            "discard" => {
                state.pending = None;
                state.device = read_profiles(&self.ch)?;
                Ok(ActionOutcome {
                    message: None,
                    reload: true,
                })
            }
            "save_onboard" => {
                let Some((p, data)) = state.pending else {
                    return Ok(ActionOutcome::default());
                };
                self.progress(false);
                let result = self.save(&mut state, p, data);
                drop(state);
                self.progress(true);
                let note = result?;
                Ok(ActionOutcome {
                    message: Some(match note {
                        None => "G600 に保存しました".into(),
                        Some(note) => format!("G600 に保存しました。{note}"),
                    }),
                    reload: true,
                })
            }
            _ => Err(HubError::Unsupported("G600 action")),
        }
    }
}

#[cfg(test)]
#[path = "g600_tests.rs"]
mod tests;
