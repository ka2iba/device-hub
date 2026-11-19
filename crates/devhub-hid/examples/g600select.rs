//! Logitech G600: アクティブなオンボードプロファイルを切り替えるだけの復旧用ツール。
//! プロファイル内容 (0xF3-F5) は一切書かない。送るのは 0xF0 の 4 バイト 1 回だけ。
//!
//! 使い方: cargo run -p devhub-hid --example g600select -- <0|1|2>

use hidapi::HidApi;

fn status(dev: &hidapi::HidDevice) -> Option<u8> {
    let mut buf = [0u8; 256];
    buf[0] = 0xF0;
    match dev.get_feature_report(&mut buf) {
        Ok(n) if n >= 4 => {
            println!("GET 0xF0: {:02X?}", &buf[..4]);
            Some(buf[1] >> 4)
        }
        Ok(n) => {
            eprintln!("GET 0xF0: short report ({n} bytes)");
            None
        }
        Err(e) => {
            eprintln!("GET 0xF0: {e}");
            None
        }
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
    let Some(info) = api.device_list().find(|d| {
        d.vendor_id() == 0x046D && d.product_id() == 0xC24A && d.usage_page() == 0xFF80
    }) else {
        eprintln!("G600 の設定用コレクション (usage page 0xFF80) が見つかりません");
        std::process::exit(1);
    };
    let dev = api.open_path(info.path()).expect("open G600");
    println!("before: active profile = {:?}", status(&dev));
    let report = [0xF0, 0x80 | (profile << 4), 0, 0];
    println!("SET 0xF0 {report:02X?}: {:?}", dev.send_feature_report(&report));
    println!("after: active profile = {:?}", status(&dev));
}
