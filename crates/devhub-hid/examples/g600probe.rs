//! Logitech G600 feature report probe (読み取り専用)。
//!
//! 2026-11-19: --roundtrip (同一内容の書き戻し) で F4 が 1 バイトずれて保存される破損が起きたため
//! 書き込みモードは削除した。書き込み経路の調査は docs/G600_BRIEF.md §3.4 を参照。

use hidapi::{HidApi, HidDevice};
use std::{thread, time::Duration};

const STATUS_ID: u8 = 0xf0;
const PROFILE_IDS: [u8; 3] = [0xf3, 0xf4, 0xf5];

fn read_report<const LEN: usize>(device: &HidDevice, id: u8) -> Result<[u8; LEN], String> {
    let mut buffer = [0; 256];
    buffer[0] = id;
    let len = device
        .get_feature_report(&mut buffer)
        .map_err(|err| format!("GET 0x{id:02X}: {err}"))?;
    let report = &buffer[..len];
    println!("GET 0x{id:02X}: {len} bytes {report:02X?}");
    if len < LEN || buffer[0] != id {
        return Err(format!(
            "GET 0x{id:02X}: expected ID 0x{id:02X} and at least {LEN} bytes"
        ));
    }
    // Keep the protocol payload; the returned length may include HID padding.
    Ok(buffer[..LEN].try_into().unwrap())
}

fn read_status(device: &HidDevice) -> Result<(u8, u8), String> {
    let report = read_report::<4>(device, STATUS_ID)?;
    let profile = report[1] >> 4;
    let resolution = (report[1] >> 1) & 3;
    println!("  active profile index={profile}, resolution index={resolution}");
    Ok((profile, resolution))
}

fn show_result<T>(result: Result<T, String>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(err) => {
            eprintln!("{err}");
            None
        }
    }
}

fn show_profile(report: &[u8; 154]) {
    let effect = match report[4] {
        0 => "fixed",
        1 => "breathing",
        2 => "color cycle",
        _ => "unknown",
    };
    println!(
        "  LED RGB={:02X?}, effect={} ({effect}), period={} s",
        &report[1..4],
        report[4],
        report[5]
    );
    println!("  G-Shift LED RGB={:02X?}", &report[91..94]);
    println!(
        "  polling={} Hz (raw={})",
        1000 / (u16::from(report[11]) + 1),
        report[11]
    );
    println!("  DPI shift: {}", dpi_label(report[12]));

    let stages = &report[14..18];
    let raw = report[13];
    let interpret = |index: Option<u8>| {
        index
            .map(usize::from)
            .filter(|&i| i < stages.len())
            .map_or_else(
                || "out of range".to_owned(),
                |i| format!("stage {} ({})", i + 1, dpi_label(stages[i])),
            )
    };
    println!(
        "  default DPI (offset 13): raw={raw} (0x{raw:02X}), 1-based: {}, 0-based: {}",
        interpret(raw.checked_sub(1)),
        interpret(Some(raw))
    );
    for (index, &value) in stages.iter().enumerate() {
        println!("  DPI stage {}: {}", index + 1, dpi_label(value));
    }
    for (layer, buttons) in [("Normal", &report[31..91]), ("G-Shift", &report[94..154])] {
        for (index, button) in buttons.chunks_exact(3).enumerate() {
            println!(
                "  {layer} G{}: code=0x{:02X} modifier=0x{:02X} key=0x{:02X}",
                index + 1,
                button[0],
                button[1],
                button[2]
            );
        }
    }
}

fn dpi_label(raw: u8) -> String {
    if raw == 0 {
        return "disabled".to_owned();
    }
    format!("{} dpi", u16::from(raw) * 50)
}

fn probe(device: &HidDevice) {
    println!("  Immediate reads");
    let status = show_result(read_status(device));
    let mut profile = show_result(read_report::<154>(device, PROFILE_IDS[0]));
    if status.is_none() || profile.is_none() {
        thread::sleep(Duration::from_secs(2));
        println!("  Retry after 2 seconds");
        show_result(read_status(device));
        let retried = show_result(read_report::<154>(device, PROFILE_IDS[0]));
        profile = retried.or(profile);
    }
    if let Some(profile) = profile {
        show_profile(&profile);
        for id in PROFILE_IDS.into_iter().skip(1) {
            if let Some(profile) = show_result(read_report::<154>(device, id)) {
                show_profile(&profile);
            }
        }
        // 記述子に宣言されているが用途不明の feature report (GET のみ)。
        println!("  Unknown feature reports (GET only)");
        for id in [0xF1, 0xF2, 0xF6] {
            show_result(read_report::<1>(device, id));
        }
    }
}

/// Windows が保持するレポート記述子 (preparsed data からの再構成) を表示し、
/// report ID ごとの長さ (ID 込みバイト数) に集計する。読み取りのみ。
fn show_descriptor(device: &HidDevice) {
    let mut buf = [0u8; 4096];
    let len = match device.get_report_descriptor(&mut buf) {
        Ok(len) => len,
        Err(err) => {
            eprintln!("  report descriptor: {err}");
            return;
        }
    };
    let desc = &buf[..len];
    println!("  report descriptor ({len} bytes): {desc:02X?}");
    let (mut size, mut count, mut id) = (0u32, 0u32, 0u8);
    // (kind, report id) -> bits
    let mut totals: Vec<(&str, u8, u32)> = Vec::new();
    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xFE {
            // long item: 0xFE, size, tag, data...
            let skip = desc.get(i + 1).copied().unwrap_or(0) as usize;
            i += 3 + skip;
            continue;
        }
        let n = [0, 1, 2, 4][(prefix & 3) as usize];
        let mut value = 0u32;
        for (k, b) in desc.iter().skip(i + 1).take(n).enumerate() {
            value |= u32::from(*b) << (8 * k);
        }
        match prefix & 0xFC {
            0x74 => size = value,     // Report Size
            0x94 => count = value,    // Report Count
            0x84 => id = value as u8, // Report ID
            0x80 | 0x90 | 0xB0 => {
                let kind = match prefix & 0xFC {
                    0x80 => "Input",
                    0x90 => "Output",
                    _ => "Feature",
                };
                match totals.iter_mut().find(|t| t.0 == kind && t.1 == id) {
                    Some(t) => t.2 += size * count,
                    None => totals.push((kind, id, size * count)),
                }
            }
            _ => {}
        }
        i += 1 + n;
    }
    for (kind, id, bits) in totals {
        let id_byte = u32::from(id != 0);
        println!(
            "  {kind:7} report 0x{id:02X}: {} bytes (+{} ID byte)",
            bits.div_ceil(8),
            id_byte
        );
    }
}

fn main() {
    if std::env::args().len() > 1 {
        eprintln!("Usage: g600probe (読み取り専用、引数なし)");
        std::process::exit(2);
    }

    let api = HidApi::new().expect("initialize HID");
    let collections: Vec<_> = api
        .device_list()
        .filter(|d| d.vendor_id() == 0x046d && d.product_id() == 0xc24a)
        .collect();
    for (index, info) in collections.iter().enumerate() {
        println!(
            "[{index}] 046D:C24A if={} usage_page=0x{:04X} usage=0x{:04X} path={:?}",
            info.interface_number(),
            info.usage_page(),
            info.usage(),
            info.path()
        );
    }

    if collections.is_empty() {
        println!("No Logitech G600 collections found");
    }
    for (index, info) in collections.iter().enumerate() {
        println!("Probing collection [{index}]");
        match api.open_path(info.path()) {
            Ok(device) => {
                show_descriptor(&device);
                probe(&device)
            }
            Err(err) => eprintln!("Open collection [{index}]: {err}"),
        }
    }
}
