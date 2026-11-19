use super::*;
use std::sync::Arc;

fn report(p: u8) -> Report {
    // §3.2 のレイアウトから組んだ人工データ。
    let mut data = [0; PROFILE_LEN];
    data[0] = FIRST_PROFILE + p;
    data[1..6].copy_from_slice(&[17, 51, 88, 1, 9]);
    data[6..11].copy_from_slice(&[91, 82, 73, 64, 55]);
    data[11] = 1;
    data[12] = 12;
    data[13] = 3;
    data[DPI_SLOTS].copy_from_slice(&[8, 0, 24, 0]);
    data[18..31].fill(77);
    data[31] = 1;
    data[34] = 0x15;
    data[37..40].copy_from_slice(&[0x16, 9, 25]);
    data[91..94].copy_from_slice(&[12, 24, 48]);
    data
}

fn reply_status(ch: &Channel, p: u8, ready: bool) {
    ch.mock_reply(&[STATUS, (p << 4) | (u8::from(ready) << 3), 0, 0]);
}

fn setup(p: u8) -> (G600, Arc<Mutex<Vec<HubEvent>>>) {
    let ch = Channel::mock();
    reply_status(&ch, p, true);
    for i in 0..PROFILE_COUNT {
        ch.mock_reply(&report(i));
    }
    let info = DeviceInfo {
        id: DeviceId("test-g600".into()),
        kind: DeviceKind::Mouse,
        name: "Test".into(),
        vendor: "Test".into(),
        model: "g600".into(),
        vid: 0x046D,
        pid: 0xC24A,
        transport: Transport::Wired,
        firmware: None,
        dongle_firmware: None,
        serial: None,
        driver: DriverKind::LogitechG600,
        battery: None,
        supported: true,
        asleep: false,
        board_id: None,
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = events.clone();
    let mut driver = G600::new(
        ch,
        info,
        Some(Arc::new(move |e| recorded.lock().unwrap().push(e))),
    )
    .unwrap();
    // 各テストで繰り返す初期待機だけを省略する。
    driver.opened_at -= Duration::from_secs(2);
    (driver, events)
}

fn polling(hz: i64) -> Change {
    Change::Setting {
        id: "polling".into(),
        value: SettingValue::Choice(hz),
    }
}

fn stage(x: u32) -> DpiStage {
    DpiStage {
        x,
        y: None,
        color: Rgb::default(),
        enabled: true,
    }
}

fn stage_change(stages: Vec<DpiStage>, active: usize) -> Change {
    Change::DpiStages { stages, active }
}

fn button(slot: usize, action: ButtonAction) -> Change {
    Change::Button { slot, action }
}

fn stash(driver: &G600, p: u8) -> Report {
    reply_status(&driver.ch, p, true);
    driver.ch.mock_reply(&report(p));
    driver.apply(&polling(125)).unwrap();
    let (target, data) = driver.state.lock().unwrap().pending.unwrap();
    assert_eq!(p, target);
    assert!(driver.ch.mock_sent().is_empty());
    data
}

fn ready_pair(ch: &Channel, before: u8, after: u8) {
    reply_status(ch, before, true);
    reply_status(ch, after, true);
}

fn assert_progress(events: &Mutex<Vec<HubEvent>>) {
    let flags: Vec<_> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            HubEvent::Progress {
                done,
                fraction: None,
                ..
            } => Some(*done),
            _ => None,
        })
        .collect();
    assert_eq!(flags, [false, true]);
}

#[test]
fn button_table_and_unknown_bytes_round_trip() {
    let mut cases = vec![
        ([0, 0, 0], ButtonAction::Disabled),
        (
            [0, 2, 4],
            ButtonAction::Key(KeyStroke { mods: 2, usage: 4 }),
        ),
        (
            [0, 1, 0],
            ButtonAction::Key(KeyStroke { mods: 1, usage: 0 }),
        ),
        ([0x11, 0, 0], ButtonAction::Dpi(DpiFn::Up)),
        ([0x12, 0, 0], ButtonAction::Dpi(DpiFn::Down)),
        ([0x13, 0, 0], ButtonAction::Dpi(DpiFn::Cycle)),
        ([0x14, 0, 0], ButtonAction::ProfileCycle),
        ([0x15, 0, 0], ButtonAction::Dpi(DpiFn::Shift(600))),
        ([0x17, 0, 0], ButtonAction::GShift),
        (
            [0x16, 5, 90],
            ButtonAction::Unsupported {
                ty: 0x16,
                params: vec![5, 90],
            },
        ),
    ];
    for (i, f) in MOUSE_BUTTONS.into_iter().enumerate() {
        cases.push(([i as u8 + 1, 0, 0], ButtonAction::Mouse(f)));
    }
    for (bytes, action) in cases {
        assert_eq!(decode_button(&bytes, 12), action);
        assert_eq!(encode_button(&action).unwrap(), bytes);
    }
    assert_eq!(
        decode_button(&[0x15, 0, 0], 0),
        ButtonAction::Dpi(DpiFn::Shift(200))
    );

    let original = report(0);
    let mut data = original;
    let decoded = decode_profile(0, &data, false);
    for (i, action) in decoded.mouse.unwrap().buttons.into_iter().enumerate() {
        update(&mut data, &button(i, action)).unwrap();
    }
    assert_eq!(data, original);
    update(&mut data, &polling(125)).unwrap();
    assert_eq!(data[6..11], original[6..11]);
    assert_eq!(data[18..31], original[18..31]);
}

#[test]
fn dpi_compacts_on_read_and_rewrites_stage_slots() {
    let mut data = report(0);
    let mouse = decode_profile(0, &data, false).mouse.unwrap();
    assert_eq!(mouse.stages, [stage(400), stage(1200)]);
    assert_eq!(mouse.active_stage, 1);
    update(&mut data, &Change::ActiveStage(0)).unwrap();
    assert_eq!(data[13], 1);
    update(&mut data, &Change::ActiveStage(1)).unwrap();
    assert_eq!(data[13], 3);
    update(
        &mut data,
        &stage_change(vec![stage(1), stage(926), stage(9000)], 1),
    )
    .unwrap();
    assert_eq!(data[13..18], [2, 4, 19, 164, 0]);
    for change in [
        stage_change(vec![], 0),
        stage_change(vec![stage(800); 5], 0),
        stage_change(vec![stage(800)], 1),
        Change::ActiveStage(3),
    ] {
        assert!(matches!(
            update(&mut data, &change),
            Err(HubError::Invalid(_))
        ));
    }
}

#[test]
fn polling_shift_and_lighting_conversions() {
    let mut data = report(0);
    for (hz, code) in [(125, 7), (250, 3), (500, 1), (1000, 0)] {
        update(&mut data, &polling(hz)).unwrap();
        assert_eq!(data[11], code);
        assert_eq!(
            decode_profile(0, &data, false).settings["polling"],
            SettingValue::Choice(hz)
        );
    }
    assert!(matches!(
        update(&mut data, &polling(750)),
        Err(HubError::Invalid(_))
    ));
    assert!(update(&mut data, &button(20, ButtonAction::Dpi(DpiFn::Shift(500)))).unwrap());
    assert_eq!(data[12], 10);
    assert_eq!(
        decode_profile(0, &data, false).mouse.unwrap().buttons[1],
        ButtonAction::Dpi(DpiFn::Shift(500))
    );

    data[5] = 0;
    let mut lights = decode_profile(0, &data, false).lighting;
    assert_eq!(lights[&0].speed, 14);
    let mut normal = lights.remove(&0).unwrap();
    normal.color = Rgb::new(1, 2, 3);
    normal.speed = 4;
    normal.effect = 2;
    update(
        &mut data,
        &Change::Lighting {
            area: 0,
            state: normal,
        },
    )
    .unwrap();
    assert_eq!(data[1..6], [1, 2, 3, 2, 11]);
    // 固定エフェクトは周期バイトを書き換えない (実機の固定プロファイルは周期 0)。
    data[5] = 0;
    let mut fixed = decode_profile(0, &data, false).lighting.remove(&0).unwrap();
    fixed.effect = 0;
    fixed.color = Rgb::new(4, 5, 6);
    fixed.speed = 7;
    update(
        &mut data,
        &Change::Lighting {
            area: 0,
            state: fixed,
        },
    )
    .unwrap();
    assert_eq!(data[1..6], [4, 5, 6, 0, 0]);
    let mut shifted = lights.remove(&1).unwrap();
    shifted.color = Rgb::new(9, 8, 7);
    update(
        &mut data,
        &Change::Lighting {
            area: 1,
            state: shifted,
        },
    )
    .unwrap();
    assert_eq!(data[91..94], [9, 8, 7]);
}

#[test]
fn edits_are_staged_and_undo_clears_unsaved() {
    let (driver, events) = setup(0);
    let ch = &driver.ch;
    reply_status(ch, 0, true);
    ch.mock_reply(&report(0));
    assert!(matches!(
        driver.apply(&button(0, ButtonAction::Disabled)),
        Err(HubError::Invalid(_))
    ));
    stash(&driver, 0);
    for change in [
        button(40, ButtonAction::Disabled),
        button(
            1,
            ButtonAction::Key(KeyStroke {
                mods: 0,
                usage: 256,
            }),
        ),
        button(1, ButtonAction::Mouse(MouseFn::ScrollLeft)),
        Change::Profile(1),
    ] {
        assert!(matches!(driver.apply(&change), Err(HubError::Invalid(_))));
    }
    reply_status(ch, 2, true);
    let pending = driver.read().unwrap();
    assert!(pending.unsaved);
    assert_eq!(pending.profile.active, 0);
    driver.apply(&polling(250)).unwrap();
    driver.apply(&polling(500)).unwrap();
    reply_status(ch, 2, true);
    ch.mock_reply(&report(2));
    let clean = driver.read().unwrap();
    assert!(!clean.unsaved);
    assert_eq!(clean.profile.active, 2);
    assert!(ch.mock_sent().is_empty());
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, HubEvent::StateChanged(_)))
            .count(),
        2
    );
}

#[test]
fn discard_reloads_all_profiles_using_get_only() {
    let (driver, _) = setup(0);
    stash(&driver, 0);
    reply_status(&driver.ch, 0, true);
    let mut refreshed = report(2);
    refreshed[11] = 3;
    for data in [report(0), report(1), refreshed] {
        driver.ch.mock_reply(&data);
    }
    assert!(driver.run_action("discard", None).unwrap().reload);
    let state = driver.state.lock().unwrap();
    assert!(state.pending.is_none());
    assert_eq!(state.device[2], refreshed);
    assert!(driver.ch.mock_sent().is_empty());
}

#[test]
fn profile_selection_waits_until_ready() {
    let (mut driver, _) = setup(0);
    driver.opened_at = Instant::now();
    driver.ch.mock_reply(&report(1));
    reply_status(&driver.ch, 0, false);
    reply_status(&driver.ch, 0, true);
    reply_status(&driver.ch, 1, false);
    reply_status(&driver.ch, 1, true);
    // 既定段 (offset 13 = 3 → 段 3、DPI 24) で解像度を再設定し、0xF2 で反映を確認する。
    ready_pair(&driver.ch, 1, 1);
    driver.ch.mock_reply(&[DPI_STATE, 0, 0x10, 24, 2]);
    driver.apply(&Change::Profile(1)).unwrap();
    assert!(driver.opened_at.elapsed() >= Duration::from_millis(2500));
    assert_eq!(
        driver.ch.mock_sent(),
        [vec![STATUS, 0x90, 0, 0], vec![STATUS, 0x44, 0, 0]]
    );
    assert_eq!(
        &driver.ch.mock_feature_ops()[4..],
        &[
            (false, 0xF4),
            (false, STATUS),
            (false, STATUS),
            (true, STATUS),
            (false, STATUS),
            (false, STATUS),
            (false, STATUS),
            (true, STATUS),
            (false, STATUS),
            (false, DPI_STATE),
        ]
    );
}

#[test]
fn active_save_uses_safe_fallback_and_gates_each_set() {
    let (driver, events) = setup(0);
    let data = stash(&driver, 0);
    let ch = &driver.ch;
    let start = ch.mock_feature_ops().len();
    reply_status(ch, 0, true);
    let mut unsafe_profile = report(1);
    unsafe_profile[31] = 0;
    unsafe_profile[94] = 1;
    ch.mock_reply(&unsafe_profile);
    ch.mock_reply(&report(2));
    ready_pair(ch, 0, 2);
    ready_pair(ch, 2, 2);
    ch.mock_reply(&data);
    ready_pair(ch, 2, 0);
    ch.mock_reply(&[LED_STATE, 0, 0, 0, data[4], data[5], 0, 0]);
    ch.mock_reply(&data);
    ready_pair(ch, 0, 0);
    ch.mock_reply(&[DPI_STATE, 0, 0x10, data[16], 2]);

    let outcome = driver.run_action("save_onboard", None).unwrap();
    assert!(outcome.reload);
    assert_eq!(outcome.message.as_deref(), Some("G600 に保存しました"));
    assert_eq!(
        ch.mock_sent(),
        [
            vec![STATUS, 0xA0, 0, 0],
            data.to_vec(),
            vec![STATUS, 0x80, 0, 0],
            // 既定段 3 (offset 13 = 3) の解像度再設定で DPI を即時反映する。
            vec![STATUS, 0x44, 0, 0],
        ]
    );
    assert_eq!(
        &ch.mock_feature_ops()[start..],
        &[
            (false, STATUS),
            (false, 0xF4),
            (false, 0xF5),
            (false, STATUS),
            (true, STATUS),
            (false, STATUS),
            (false, STATUS),
            (true, 0xF3),
            (false, STATUS),
            (false, 0xF3),
            (false, STATUS),
            (true, STATUS),
            (false, STATUS),
            (false, LED_STATE),
            (false, 0xF3),
            (false, STATUS),
            (true, STATUS),
            (false, STATUS),
            (false, DPI_STATE),
        ]
    );
    assert!(driver.state.lock().unwrap().pending.is_none());
    assert_progress(&events);
}

#[test]
fn dpi_not_reflected_still_counts_as_saved() {
    let (driver, events) = setup(0);
    let data = stash(&driver, 0);
    let ch = &driver.ch;
    reply_status(ch, 0, true);
    ch.mock_reply(&report(1));
    ready_pair(ch, 0, 1);
    ready_pair(ch, 1, 1);
    ch.mock_reply(&data);
    ready_pair(ch, 1, 0);
    ch.mock_reply(&[LED_STATE, 0, 0, 0, data[4], data[5], 0, 0]);
    ch.mock_reply(&data);
    ready_pair(ch, 0, 0);
    // 0xF2 が保存値と違う → 保存は成功、挿し直しを案内する。
    ch.mock_reply(&[DPI_STATE, 0, 0x10, data[16].wrapping_add(1), 2]);
    let outcome = driver.run_action("save_onboard", None).unwrap();
    assert!(outcome.message.as_deref().is_some_and(
        |m| m.starts_with("G600 に保存しました。DPI の変更は USB を挿し直すと反映されます")
    ));
    assert!(driver.state.lock().unwrap().pending.is_none());
    assert_progress(&events);
}

#[test]
fn invalid_default_stage_sends_no_resolution() {
    let (driver, _) = setup(0);
    let mut data = report(0);
    data[13] = 2; // 段 2 は DPI 0 (無効)
    assert!(matches!(
        driver.apply_resolution(&data),
        Err(HubError::Protocol(_))
    ));
    data[13] = 0; // 範囲外
    assert!(matches!(
        driver.apply_resolution(&data),
        Err(HubError::Protocol(_))
    ));
    assert!(driver.ch.mock_sent().is_empty());
}

#[test]
fn no_safe_fallback_means_no_set() {
    let (driver, events) = setup(0);
    stash(&driver, 0);
    let ch = &driver.ch;
    reply_status(ch, 0, true);
    let mut no_dpi = report(1);
    no_dpi[DPI_SLOTS].fill(0);
    let mut no_left = report(2);
    no_left[31] = 0;
    ch.mock_reply(&no_dpi);
    ch.mock_reply(&no_left);
    assert!(matches!(
        driver.run_action("save_onboard", None),
        Err(HubError::Protocol(_))
    ));
    assert!(ch.mock_sent().is_empty());
    assert_progress(&events);
}

#[test]
fn inactive_save_only_writes_target() {
    let (driver, events) = setup(1);
    let data = stash(&driver, 1);
    let ch = &driver.ch;
    reply_status(ch, 2, true);
    ready_pair(ch, 2, 2);
    ch.mock_reply(&data);
    assert!(driver.run_action("save_onboard", None).unwrap().reload);
    assert_eq!(ch.mock_sent(), [data.to_vec()]);
    assert_progress(&events);
}

#[test]
fn verify_mismatch_retries_once_and_leaves_fallback_active() {
    let (driver, events) = setup(0);
    let data = stash(&driver, 0);
    let ch = &driver.ch;
    reply_status(ch, 0, true);
    ch.mock_reply(&report(1));
    ready_pair(ch, 0, 1);
    for _ in 0..2 {
        ready_pair(ch, 1, 1);
        ch.mock_reply(&report(0));
    }
    assert!(matches!(
        driver.run_action("save_onboard", None),
        Err(HubError::Protocol(_))
    ));
    assert_eq!(
        ch.mock_sent(),
        [vec![STATUS, 0x90, 0, 0], data.to_vec(), data.to_vec()]
    );
    assert_eq!(driver.state.lock().unwrap().pending, Some((0, data)));
    assert_progress(&events);
}

#[test]
fn activation_check_failure_restores_fallback() {
    for led_matches in [false, true] {
        let (driver, events) = setup(0);
        let data = stash(&driver, 0);
        let ch = &driver.ch;
        reply_status(ch, 0, true);
        ch.mock_reply(&report(1));
        ready_pair(ch, 0, 1);
        ready_pair(ch, 1, 1);
        ch.mock_reply(&data);
        ready_pair(ch, 1, 0);
        let period = if led_matches { data[5] } else { data[5] + 1 };
        ch.mock_reply(&[LED_STATE, 0, 0, 0, data[4], period, 0, 0]);
        if led_matches {
            let mut unsafe_profile = data;
            unsafe_profile[31] = 0;
            ch.mock_reply(&unsafe_profile);
        }
        ready_pair(ch, 0, 1);
        assert!(matches!(
            driver.run_action("save_onboard", None),
            Err(HubError::Protocol(_))
        ));
        assert_eq!(
            ch.mock_sent(),
            [
                vec![STATUS, 0x90, 0, 0],
                data.to_vec(),
                vec![STATUS, 0x80, 0, 0],
                vec![STATUS, 0x90, 0, 0],
            ]
        );
        assert!(driver.state.lock().unwrap().pending.is_some());
        assert_progress(&events);
    }
}

#[test]
fn target_that_became_active_is_never_written() {
    let (driver, events) = setup(0);
    stash(&driver, 0);
    let ch = &driver.ch;
    reply_status(ch, 0, true);
    ch.mock_reply(&report(1));
    ready_pair(ch, 0, 1);
    reply_status(ch, 0, true);
    assert!(matches!(
        driver.run_action("save_onboard", None),
        Err(HubError::Protocol(_))
    ));
    assert_eq!(ch.mock_sent(), [vec![STATUS, 0x90, 0, 0]]);
    assert_progress(&events);
}

#[test]
fn busy_timeout_sends_no_set() {
    let (driver, _) = setup(0);
    driver.ch.mock_reply(&report(1));
    for _ in 0..40 {
        reply_status(&driver.ch, 0, false);
    }
    let start = Instant::now();
    let result = driver.apply(&Change::Profile(1));
    assert!(matches!(result, Err(HubError::Protocol(ref msg)) if msg.contains("タイムアウト")));
    assert!(start.elapsed() >= Duration::from_secs(3));
    assert!(driver.ch.mock_sent().is_empty());
}

#[test]
fn status_fields_and_numbered_reply_validation() {
    assert_eq!(decode_status(0x19).unwrap(), (1, 0, true));
    assert_eq!(decode_status(0x26).unwrap(), (2, 3, false));
    assert!(matches!(decode_status(0x30), Err(HubError::Protocol(_))));
    let ch = Channel::mock();
    ch.mock_reply(&[STATUS, 8, 0]);
    assert!(matches!(status(&ch), Err(HubError::Protocol(_))));
    ch.mock_reply(&[LED_STATE, 8, 0, 0]);
    assert!(matches!(status(&ch), Err(HubError::Protocol(_))));
}

#[test]
fn failed_activation_set_waits_before_recovering() {
    let (driver, events) = setup(0);
    let data = stash(&driver, 0);
    let ch = &driver.ch;
    reply_status(ch, 0, true);
    ch.mock_reply(&report(1));
    ready_pair(ch, 0, 1);
    ready_pair(ch, 1, 1);
    ch.mock_reply(&data);
    ch.mock_fail_send(&[STATUS, 0x80]);
    reply_status(ch, 1, true); // 元に戻す SET の直前
    reply_status(ch, 0, false); // エラーを返していても本体が処理を開始した
    reply_status(ch, 0, true);
    ready_pair(ch, 0, 1);
    assert!(matches!(
        driver.run_action("save_onboard", None),
        Err(HubError::Protocol(_))
    ));
    let ops = ch.mock_feature_ops();
    assert_eq!(
        ops[ops.len() - 7..],
        [
            (false, STATUS),
            (true, STATUS),
            (false, STATUS),
            (false, STATUS),
            (false, STATUS),
            (true, STATUS),
            (false, STATUS),
        ]
    );
    assert_eq!(ch.mock_sent().last().unwrap(), &[STATUS, 0x90, 0, 0]);
    assert!(driver.state.lock().unwrap().pending.is_some());
    assert_progress(&events);
}

#[test]
fn resolution_is_not_sent_when_profile_changed_in_between() {
    // プロファイル 2 を選んだ直後、再設定の前に本体ボタンでプロファイル 3 に変わった場合。
    let (driver, _) = setup(0);
    let ch = &driver.ch;
    ch.mock_reply(&report(1));
    ready_pair(ch, 0, 1);
    reply_status(ch, 2, true);
    let result = driver.apply(&Change::Profile(1));
    assert!(matches!(result, Err(HubError::Protocol(ref m)) if m.contains("中止")));
    // 選択の SET だけで、別プロファイル宛ての解像度 SET (0x44) は送らない。
    assert_eq!(ch.mock_sent(), [vec![STATUS, 0x90, 0, 0]]);
}
