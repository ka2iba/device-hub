//! Logitech G600 書き込み実験 E1 (docs/G600_BRIEF.md §3.6)。
//!
//! 使われていないプロファイル 1 (report 0xF4) にだけ、保存してある元の内容を 1 回書く。
//! - 書き込み先は 0xF4 固定。プロファイル 1 がアクティブなら中止する。
//! - 元データは Git 管理外のファイル (既定: .private-analysis/g600/F4_original_2026-11-19.txt)。
//!
//! 使い方:
//!   g600e1            … 確認のみ (書かない)。今の F4 と元データの差分を表示
//!   g600e1 write      … 2 秒待ってから 0xF4 を 1 回書き、直後の GET を表示
//!   g600e1 verify     … 読み取りのみ。USB を挿し直した後に保存内容を確認する
//!   g600e1 e4         … E4: 0xF4 を 1 回書き、0xF0 の bit3 の時間変化と
//!                       「現在のプロファイルの再選択」で再読み込みが起きるかを観察する
//!   (末尾に元データのパスを渡すと既定のパスの代わりに使う)

use hidapi::{HidApi, HidDevice};
use std::{thread, time::Duration};

const TARGET_ID: u8 = 0xF4;
const LEN: usize = 154;
const DEFAULT_SRC: &str = ".private-analysis/g600/F4_original_2026-11-19.txt";

fn load_original(path: &str) -> [u8; LEN] {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("元データを読めません ({path}): {e}");
        std::process::exit(1);
    });
    let bytes: Vec<u8> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(|l| l.split_whitespace())
        .map(|h| {
            u8::from_str_radix(h, 16).unwrap_or_else(|_| {
                eprintln!("16 進ではない値: {h:?}");
                std::process::exit(1);
            })
        })
        .collect();
    let Ok(arr) = <[u8; LEN]>::try_from(bytes.as_slice()) else {
        eprintln!("元データは {LEN} バイト必要です (実際 {})", bytes.len());
        std::process::exit(1);
    };
    if arr[0] != TARGET_ID {
        eprintln!("元データの先頭が 0x{TARGET_ID:02X} ではありません");
        std::process::exit(1);
    }
    arr
}

fn get(dev: &HidDevice, id: u8, len: usize) -> Option<Vec<u8>> {
    let mut buf = [0u8; 256];
    buf[0] = id;
    match dev.get_feature_report(&mut buf) {
        Ok(n) if n >= len && buf[0] == id => Some(buf[..len].to_vec()),
        Ok(n) => {
            eprintln!("GET 0x{id:02X}: 想定外の応答 ({n} bytes)");
            None
        }
        Err(e) => {
            eprintln!("GET 0x{id:02X}: {e}");
            None
        }
    }
}

/// bit3==1 (待機中) を最大 3 秒待つ (§3.8)。待てなければ終了する。
fn wait_ready(dev: &HidDevice) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(s) = get(dev, 0xF0, 4) {
            if s[1] & 8 != 0 {
                return;
            }
        }
        if std::time::Instant::now() > deadline {
            eprintln!("本体が保存処理中のまま (bit3=0) か、0xF0 を読めません。送らずに終了します");
            std::process::exit(1);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// 0xF0 を読んで表示し、アクティブなプロファイル番号を返す。
fn status(dev: &HidDevice) -> Option<u8> {
    let s = get(dev, 0xF0, 4)?;
    let b = s[1];
    println!(
        "  0xF0 = {s:02X?}  profile={} res={} bit0={} bit3={}",
        b >> 4,
        (b >> 1) & 3,
        b & 1,
        (b >> 3) & 1
    );
    Some(b >> 4)
}

/// 元データとの比較。1 バイト挿入型のずれも判定する。
fn compare(label: &str, got: &[u8], want: &[u8; LEN]) {
    let diffs: Vec<usize> = (0..LEN).filter(|&i| got[i] != want[i]).collect();
    if diffs.is_empty() {
        println!("  {label}: 元データと完全一致");
        return;
    }
    println!(
        "  {label}: {} バイト不一致 (最初の不一致 offset {})",
        diffs.len(),
        diffs[0]
    );
    let k = diffs[0];
    if k > 0 && got[k + 1..] == want[k..LEN - 1] {
        println!(
            "    → offset {k} に 1 バイト挿入されたずれ (挿入値 0x{:02X})",
            got[k]
        );
    }
    println!("  {label} 全体: {got:02X?}");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("check");
    if !matches!(mode, "check" | "write" | "verify" | "e4") {
        eprintln!("usage: g600e1 [write|verify|e4] [元データのパス]");
        std::process::exit(2);
    }
    let src = args.get(1).map(String::as_str).unwrap_or(DEFAULT_SRC);
    let want = load_original(src);

    let api = HidApi::new().expect("initialize HID");
    let Some(info) = api
        .device_list()
        .find(|d| d.vendor_id() == 0x046D && d.product_id() == 0xC24A && d.usage_page() == 0xFF80)
    else {
        eprintln!("G600 の設定用コレクション (usage page 0xFF80) が見つかりません");
        std::process::exit(1);
    };
    let dev = api.open_path(info.path()).expect("open G600");

    println!("[before]");
    let Some(active) = status(&dev) else {
        std::process::exit(1)
    };
    if let Some(cur) = get(&dev, TARGET_ID, LEN) {
        compare("現在の F4", &cur, &want);
    }

    match mode {
        "check" => println!("確認のみ (書き込みなし)。書くときは `g600e1 write`"),
        "verify" => println!("読み取りのみ (verify)。"),
        _ => {
            if active == 1 {
                eprintln!("プロファイル 1 (F4) がアクティブなので中止します。先に `g600select -- 0` を実行してください");
                std::process::exit(1);
            }
            println!("2 秒後に 0xF4 を 1 回書きます。マウスに触らないでください…");
            thread::sleep(Duration::from_secs(2));
            wait_ready(&dev);
            assert_eq!(want[0], TARGET_ID, "書き込み先は 0xF4 固定");
            let sent = dev.send_feature_report(&want);
            println!("SET 0x{TARGET_ID:02X} ({LEN} bytes): {sent:?}");
            thread::sleep(Duration::from_millis(500));
            println!("[直後の GET — 送った内容が返るだけの可能性あり]");
            status(&dev);
            if let Some(cur) = get(&dev, TARGET_ID, LEN) {
                compare("直後の F4", &cur, &want);
            }
            if mode == "e4" {
                e4(&dev, active, &want);
            }
            println!("次に USB を挿し直し、`g600e1 verify` で保存内容を確認してください。");
        }
    }
}

/// 0xF0 を `ms` ミリ秒間 100ms ごとに読み、b1 が変わった時刻だけ表示する。
fn watch_status(dev: &HidDevice, ms: u64) {
    let start = std::time::Instant::now();
    let mut last: Option<u8> = None;
    while start.elapsed() < Duration::from_millis(ms) {
        if let Some(s) = get(dev, 0xF0, 4) {
            if last != Some(s[1]) {
                println!(
                    "    t={:>4}ms  0xF0 b1=0x{:02X} bit3={}",
                    start.elapsed().as_millis(),
                    s[1],
                    (s[1] >> 3) & 1
                );
                last = Some(s[1]);
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// E4: 書いた後の bit3 の推移と、現在のプロファイル再選択の効果を見る (書き込み後に呼ぶ)。
fn e4(dev: &HidDevice, active: u8, want: &[u8; LEN]) {
    println!("[E4-1] 書き込み後 3 秒間、何もせずに 0xF0 を観察");
    watch_status(dev, 3000);
    println!("[E4-2] 他のプロファイルの GET (F3/F5 が自分の内容を返すか)");
    for id in [0xF3, 0xF5] {
        if let Some(r) = get(dev, id, LEN) {
            println!("  GET 0x{id:02X} 先頭16B: {:02X?}", &r[..16]);
        }
    }
    if let Some(cur) = get(dev, TARGET_ID, LEN) {
        compare("F4", &cur, want);
    }
    let select = [0xF0, 0x80 | (active << 4), 0, 0];
    println!("[E4-3] 現在のプロファイル {active} を再選択 {select:02X?}");
    wait_ready(dev);
    println!("  SET: {:?}", dev.send_feature_report(&select));
    watch_status(dev, 2000);
    if let Some(cur) = get(dev, TARGET_ID, LEN) {
        compare("再選択後の F4", &cur, want);
    }
}
