//! Logitech G600: アクティブなオンボードプロファイルを切り替えるだけの復旧用ツール。
//! プロファイル内容 (0xF3-F5) は一切書かない。送るのは 0xF0 の 4 バイト 1 回だけ。
//!
//! 送る前後に docs/G600_BRIEF.md §3.8 の手順を守る (open から 2 秒待つ・0xF0 の bit3==1
//! (待機中) を待つ)。保存処理中への割り込みは §3.5 の破損事故の最有力原因のため。
//!
//! 使い方: cargo run -p devhub-hid --example g600select -- <0|1|2>

use hidapi::{HidApi, HidDevice};
use std::{thread, time::Duration, time::Instant};

/// 0xF0 の 2 バイト目。読めなければ None。
fn status(dev: &HidDevice) -> Option<u8> {
    let mut buf = [0u8; 256];
    buf[0] = 0xF0;
    match dev.get_feature_report(&mut buf) {
        Ok(n) if n >= 4 && buf[0] == 0xF0 => {
            println!("GET 0xF0: {:02X?}", &buf[..4]);
            Some(buf[1])
        }
        other => {
            eprintln!("GET 0xF0 に失敗: {other:?}");
            None
        }
    }
}

/// bit3==1 (待機中) を最大 3 秒待つ。待てなければ None。
fn wait_ready(dev: &HidDevice) -> Option<u8> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let b = status(dev)?;
        if b & 8 != 0 {
            return Some(b);
        }
        if Instant::now() > deadline {
            eprintln!("本体が保存処理中のまま (bit3=0) です");
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn main() {
    let profile: u8 = match std::env::args().nth(1).and_then(|s| s.parse().ok()) {
        Some(p @ 0..=2) => p,
        _ => {
            eprintln!("usage: g600select <0|1|2>");
            std::process::exit(2);
        }
    };
    let api = HidApi::new().expect("initialize HID");
    let Some(info) = api
        .device_list()
        .find(|d| d.vendor_id() == 0x046D && d.product_id() == 0xC24A && d.usage_page() == 0xFF80)
    else {
        eprintln!("G600 の設定用コレクション (usage page 0xFF80) が見つかりません");
        std::process::exit(1);
    };
    let dev = api.open_path(info.path()).expect("open G600");

    println!("2 秒待ってから、本体が待機中であることを確かめて送ります…");
    thread::sleep(Duration::from_secs(2));
    let Some(before) = wait_ready(&dev) else {
        eprintln!("送らずに終了します。少し待ってから再実行するか、USB を挿し直してください");
        std::process::exit(1);
    };
    println!("before: active profile = {}", before >> 4);

    let report = [0xF0, 0x80 | (profile << 4), 0, 0];
    println!(
        "SET 0xF0 {report:02X?}: {:?}",
        dev.send_feature_report(&report)
    );
    // 送信がエラーでも本体が処理を始めている可能性があるので、結果にかかわらず待つ。
    thread::sleep(Duration::from_millis(300));
    match wait_ready(&dev) {
        Some(after) if after >> 4 == profile => {
            println!("after: active profile = {profile} (切替完了)")
        }
        Some(after) => eprintln!(
            "after: active profile = {} (切り替わっていません)",
            after >> 4
        ),
        None => eprintln!("切替後の状態を確認できませんでした。USB を挿し直してください"),
    }
}
