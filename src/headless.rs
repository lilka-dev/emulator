use anyhow::{Result, ensure};
use lilka_emulator::qemu_live::{Config, HEIGHT, Session, WIDTH};
use std::{
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};
pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(2).collect();
    ensure!(
        args.len() == 8 || (args.len() == 10 && (args[8] == "--app" || args[8] == "--resume")),
        "usage: qemu_live_bench QEMU BRIDGE ROM_DIR BASE_FLASH SD STATE_DIR SCENARIO SECONDS [--app APP.bin | --resume SESSION_DIR]"
    );
    let config = Config {
        binary: PathBuf::from(&args[0]),
        bridge: PathBuf::from(&args[1]),
        rom_dir: PathBuf::from(&args[2]),
        firmware: PathBuf::from(&args[3]),
        sd: PathBuf::from(&args[4]),
        state_dir: PathBuf::from(&args[5]),
        wifi_ssid: std::env::var("LILKA_QEMU_WIFI_SSID").ok(),
    };
    let mut session = if args.len() == 10 && args[8] == "--resume" {
        Session::resume(config, &PathBuf::from(&args[9]))?
    } else if args.len() == 10 {
        Session::start_app(config, &PathBuf::from(&args[9]))?
    } else {
        Session::start(config)?
    };
    let scenario = args[6].to_str().unwrap_or("");
    let seconds: f64 = args[7].to_str().unwrap_or("").parse()?;
    let mut events = Vec::new();
    let mut key = |ms: u64, pin: u8| {
        events.push((ms * 240000, pin, false));
        events.push(((ms + 120) * 240000, pin, true));
    };
    match scenario {
        "menu" | "reset" => {}
        "catalog" => {
            key(15000, 5);
            key(15700, 41);
            key(16400, 41);
            key(17100, 5);
            key(18500, 5);
            key(75000, 5);
            key(90000, 5);
            key(105000, 5);
            key(108000, 5);
            key(112000, 5);
            for (ms, pin) in [(114000, 38), (115000, 39), (116000, 41), (117000, 40)] {
                key(ms, pin);
            }
        }
        "wifi" | "wifi-connect" | "wifi-connect-reset" => {
            for ms in [7000, 7400, 7800, 8200] {
                key(ms, 41);
            }
            for ms in [8800, 9500, 10200] {
                key(ms, 5);
            }
            key(14000, 41);
            key(14700, 5);
            if scenario.starts_with("wifi-connect") {
                key(24000, 5);
                key(26000, 5);
            }
        }
        "doom" => {
            key(2000, 5);
            key(40000, 0);
            for ms in [41000, 42000, 43000] {
                key(ms, 4);
            }
            for ms in (45000..80000).step_by(3000) {
                key(ms, 6);
            }
            events.push((45000 * 240000, 38, false));
            events.push((80000 * 240000, 38, true));
            events.push((46000 * 240000, 5, false));
            events.push((80000 * 240000, 5, true));
            for ms in (47000..77000).step_by(6000) {
                events.push((ms * 240000, 40, false));
                events.push(((ms + 1200) * 240000, 40, true));
            }
        }
        "asteroids" => {
            key(7000, 41);
            key(7600, 5);
            key(8300, 5);
            key(8800, 41);
            key(9200, 41);
            key(9800, 5);
            key(12500, 0);
            key(14000, 4);
            for ms in (15000..35000).step_by(400) {
                key(ms, 5);
            }
            events.push((15000 * 240000, 38, false));
            events.push((35000 * 240000, 38, true));
            events.push((16000 * 240000, 40, false));
            events.push((32000 * 240000, 40, true));
        }
        "lua" | "lua-input" => {
            key(7000, 41);
            for ms in [7600, 8300, 9000] {
                key(ms, 5);
            }
            if scenario == "lua-input" {
                for pin in [38, 41, 39, 40, 4] {
                    key(11000, pin);
                }
            }
        }
        "spiffs" => {
            key(7000, 41);
            key(7600, 5);
            key(8300, 41);
            key(9000, 5);
            key(9700, 5);
        }
        "lines" | "disk" | "transform" | "ball" | "cube" | "epilepsy" | "petpet" => {
            key(7000, 5);
            key(7600, 5);
            let index = [
                "lines",
                "disk",
                "transform",
                "ball",
                "cube",
                "epilepsy",
                "petpet",
            ]
            .iter()
            .position(|name| *name == scenario)
            .unwrap();
            for i in 0..index as u64 {
                key(8300 + i * 400, 41);
            }
            key(8500 + index as u64 * 400, 5);
        }
        "wav" | "mp3" => {
            key(7000, 41);
            for ms in [7600, 8300, 9000] {
                key(ms, 5);
            }
            if scenario == "mp3" {
                key(9600, 41);
                key(10000, 41);
                key(11000, 5);
            } else {
                key(10200, 5);
            }
        }
        _ => anyhow::bail!("Unknown scenario"),
    }
    events.sort_by_key(|e| e.0);
    let mut next = 0;
    let mut seq = 0;
    let mut rgba = Vec::new();
    let mut audio = Vec::new();
    let mut bytes = Vec::new();
    let mut pcm = std::fs::File::create(session.directory.join("client-audio.pcm"))?;
    let started = Instant::now();
    let mut frames = 0;
    let mut images = 0;
    let mut pause_start = None;
    let mut pause_done = false;
    let mut reset_done = false;
    let snapshots = std::env::var_os("LILKA_BENCH_SNAPSHOTS").is_some();
    let mut next_snapshot = 8;
    let mut timeline = Vec::new();
    let mut next_sample = 1.0;
    let deadline = started + Duration::from_secs_f64(seconds + 30.0);
    loop {
        if let Some(code) = session.exited()? {
            anyhow::bail!("CPU exited {code}; {}", session.directory.display());
        }
        let stats = session.ipc.stats();
        let guest_seconds = stats.ticks as f64 / 240_000_000.0;
        if guest_seconds >= next_sample {
            timeline.push(serde_json::json!({
                "guest_seconds": guest_seconds,
                "wall_seconds": started.elapsed().as_secs_f64(),
                "lcd_pixels": stats.lcd_pixels,
                "lcd_windows": stats.lcd_windows,
                "lcd_changed_windows": stats.lcd_changed_windows,
                "screen_updates": images,
            }));
            next_sample = guest_seconds.floor() + 1.0;
        }
        if !reset_done
            && ((scenario == "reset" && stats.ticks >= 8 * 240_000_000)
                || (scenario == "wifi-connect-reset" && stats.ticks >= 32 * 240_000_000))
        {
            session.request_guest_reset()?;
            reset_done = true;
        }
        while next < events.len() && events[next].0 <= stats.ticks {
            let (_, pin, high) = events[next];
            session.ipc.set_pin(pin, high);
            next += 1;
        }
        if !pause_done && pause_start.is_none() && stats.ticks >= 3 * 240_000_000 {
            session.pause(true);
            pause_start = Some((Instant::now(), stats.ticks));
        }
        if let Some((at, previous)) = pause_start {
            if at.elapsed() >= Duration::from_millis(350) {
                ensure!(
                    stats.ticks - previous < 240_000_000 / 20,
                    "Pause failed to stop guest clock"
                );
                session.pause(false);
                pause_start = None;
                pause_done = true;
            }
        }
        session.ipc.drain_audio(&mut audio);
        frames += audio.len();
        bytes.clear();
        for sample in &audio {
            bytes.extend_from_slice(&sample[0].to_le_bytes());
            bytes.extend_from_slice(&sample[1].to_le_bytes());
        }
        pcm.write_all(&bytes)?;
        if session.ipc.read_rgba(&mut seq, &mut rgba) {
            images += 1;
            if snapshots && stats.ticks >= next_snapshot * 240_000_000 {
                let mut ppm = format!("P6\n{WIDTH} {HEIGHT}\n255\n").into_bytes();
                for pixel in rgba.chunks_exact(4) {
                    ppm.extend_from_slice(&pixel[..3]);
                }
                std::fs::write(
                    session
                        .directory
                        .join(format!("frame-{next_snapshot:02}.ppm")),
                    ppm,
                )?;
                next_snapshot += 1;
            }
        }
        if stats.ticks as f64 / 240_000_000.0 >= seconds {
            break;
        }
        ensure!(Instant::now() < deadline, "Live CPU progress timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
    session.stop()?;
    if scenario == "reset" {
        let board: serde_json::Value =
            serde_json::from_slice(&std::fs::read(session.directory.join("board.json"))?)?;
        let uart = std::fs::read_to_string(session.directory.join("uart.log"))?;
        ensure!(
            reset_done
                && board["system_resets"].as_u64().unwrap_or(0) >= 1
                && uart.matches("ESP-ROM:").count() >= 2,
            "Guest did not reboot successfully"
        );
    }
    let uart = std::fs::read_to_string(session.directory.join("uart.log"))?;
    ensure!(
        !uart.contains("Guru Meditation"),
        "Guest firmware crashed; inspect uart.log"
    );
    session.ipc.drain_audio(&mut audio);
    frames += audio.len();
    bytes.clear();
    for sample in &audio {
        bytes.extend_from_slice(&sample[0].to_le_bytes());
        bytes.extend_from_slice(&sample[1].to_le_bytes());
    }
    pcm.write_all(&bytes)?;
    let stats = session.ipc.stats();
    let mut ppm = format!("P6\n{WIDTH} {HEIGHT}\n255\n").into_bytes();
    for pixel in rgba.chunks_exact(4) {
        ppm.extend_from_slice(&pixel[..3]);
    }
    std::fs::write(session.directory.join("client-display.ppm"), ppm)?;
    let result = serde_json::json!({"virtual_seconds":stats.ticks as f64/240_000_000.0,"wall_seconds":started.elapsed().as_secs_f64(),
        "pcm_frames":frames,"screen_updates":images,"ipc_dropped_frames":stats.dropped_frames,
        "scripted_inputs_remaining":events.len()-next,"pause_verified":pause_done,"session":session.directory,"timeline":timeline});
    std::fs::write(
        session.directory.join("client-result.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    ensure!(stats.dropped_frames == 0, "Live PCM queue overflow");
    Ok(())
}
