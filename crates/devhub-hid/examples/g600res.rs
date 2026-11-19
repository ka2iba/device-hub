//! Logitech G600 実験 E5: 現在の DPI 段の再設定 (0xF0 `[F0, 40|(r<<1), 0, 0]`) で、
//! 保存済みの新しい DPI が挿し直しなしで反映されるかを見る (docs/G600_BRIEF.md §3.9)。
//!
//! - プロファイル (0xF3-F5) には一切書かない。送るのは 0xF0 の 4 バイト 1 回だけ。
//! - アクティブなプロファイルで DPI が 0 (無効) の段は指定できない。
//! - 送る前後は §3.8 の手順 (open から 2 秒・bit3==1 待ち) を守る。
//! - 0xF2 の 4 バイト目 = 現在効いている DPI (×50) と推定 (2026-11-19 の観察)。
//!
//! 使い方: cargo run -p devhub-hid --example g600res -- <段番号 1-4>

use hidapi::{HidApi, HidDevice};
use std::{thread, time::Duration, time::Instant};

fn get(dev: &HidDevice, id: u8, len: usize) -> Vec<u8> {
    let mut buf = [0u8; 256];
    buf[0] = id;
    match dev.get_feature_report(&mut buf) {
        Ok(n) if n >= len && buf[0] == id => buf[..len].to_vec(),
        other => {
            eprintln!("GET 0x{id:02X} に失敗: {other:?}");
            std::process::exit(1);
        }
    }
}

/// bit3==1 (待機中) を最大 3 秒待つ。返り値は 0xF0 の b1。
fn wait_ready(dev: &HidDevice) -> u8 {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let b = get(dev, 0xF0, 4)[1];
        if b & 8 != 0 {
            return b;
        }
        if Instant::now() > deadline {
            eprintln!("bit3 が 1 になりません (0xF0 b1=0x{b:02X})。送らずに終了します");
            std::process::exit(1);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn show(dev: &HidDevice, label: &str) {
    let s = get(dev, 0xF0, 4);
    let f2 = get(dev, 0xF2, 5);
    println!(
        "[{label}] 0xF0 b1=0x{:02X} (profile={} res={} bit3={})  0xF2={:02X?} → 今効いている DPI ≈ {}",
        s[1],
        s[1] >> 4,
        (s[1] >> 1) & 3,
        (s[1] >> 3) & 1,
        f2,
        u32::from(f2[3]) * 50
    );
}

fn main() {
    let stage: u8 = match std::env::args().nth(1).and_then(|s| s.parse().ok()) {
        Some(n @ 1..=4) => n,
        _ => {
            eprintln!("usage: g600res <段番号 1-4>");
            std::process::exit(2);
        }
    };
    let r = stage - 1;
    let api = HidApi::new().expect("initialize HID");
    let Some(info) = api
        .device_list()
        .find(|d| d.vendor_id() == 0x046D && d.product_id() == 0xC24A && d.usage_page() == 0xFF80)
    else {
        eprintln!("G600 の設定用コレクション (usage page 0xFF80) が見つかりません");
        std::process::exit(1);
    };
    let dev = api.open_path(info.path()).expect("open G600");

    show(&dev, "before");
    let active = get(&dev, 0xF0, 4)[1] >> 4;
    if active > 2 {
        eprintln!("アクティブなプロファイル番号 {active} が不正です");
        std::process::exit(1);
    }
    let profile = get(&dev, 0xF3 + active, 154);
    let dpi = profile[14 + usize::from(r)];
    println!(
        "アクティブ: プロファイル {} / 段 {stage} の保存値 = {} dpi",
        active + 1,
        u32::from(dpi) * 50
    );
    if dpi == 0 {
        eprintln!("段 {stage} は無効 (DPI 0) なので中止します");
        std::process::exit(1);
    }

    println!(
        "2 秒後に 0xF0 [F0, {:02X}, 00, 00] を 1 回送ります。マウスに触らないでください…",
        0x40 | (r << 1)
    );
    thread::sleep(Duration::from_secs(2));
    wait_ready(&dev);
    let report = [0xF0, 0x40 | (r << 1), 0, 0];
    println!(
        "SET 0xF0 {report:02X?}: {:?}",
        dev.send_feature_report(&report)
    );
    thread::sleep(Duration::from_millis(300));
    wait_ready(&dev);
    show(&dev, "after");
    println!("マウスの感度も体感で確認してください。");
}
