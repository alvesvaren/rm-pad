mod config;
mod dump;
mod event;
mod pen;
mod ssh;
mod touch;

use std::path::Path;
use std::thread;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("dump") {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
        let key_path = Path::new(config::KEY_PATH);
        match args.get(2).map(|s| s.as_str()) {
            Some("touch") => return dump::run_dump_touch(key_path),
            Some("pen") => return dump::run_dump_pen(key_path),
            _ => {
                eprintln!("Usage: {} dump <touch|pen>", args.get(0).unwrap_or(&"rm-mouse".into()));
                eprintln!("  Streams and prints raw input events from the reMarkable for debugging.");
                std::process::exit(1);
            }
        }
    }

    let touch_only = args.iter().any(|a| a == "--touch-only");
    let pen_only = args.iter().any(|a| a == "--pen-only");
    let relative_touch = args.iter().any(|a| a == "--relative-touch");
    let run_pen = !touch_only;
    let run_touch = !pen_only;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!(
        "rm-mouse starting (host={}, pen={}, touch={})",
        config::HOST,
        if run_pen { config::PEN_DEVICE } else { "off" },
        if run_touch { config::TOUCH_DEVICE } else { "off" }
    );

    let key_path = Path::new(config::KEY_PATH);

    if !run_pen && !run_touch {
        eprintln!(
            "Usage: {} [--pen-only] [--touch-only] [--relative-touch]",
            args.get(0).unwrap_or(&"rm-mouse".into())
        );
        eprintln!("  Default: run both pen and touch. --relative-touch: use REL mouse instead of MT touchpad (works if MT fails sanity checks).");
        std::process::exit(1);
    }

    let pen_handle = if run_pen {
        let key_pen = key_path.to_path_buf();
        Some(thread::spawn(move || {
            loop {
                log::info!("[pen] thread starting…");
                if let Err(e) = pen::run(&key_pen) {
                    log::error!("[pen] {}", e);
                }
                log::warn!("[pen] disconnected, reconnecting in 2s…");
                thread::sleep(Duration::from_secs(2));
            }
        }))
    } else {
        None
    };

    let touch_handle = if run_touch {
        let key_touch = key_path.to_path_buf();
        let rel = relative_touch;
        Some(thread::spawn(move || {
            loop {
                log::info!("[touch] thread starting…");
                if let Err(e) = touch::run(&key_touch, rel) {
                    log::error!("[touch] {}", e);
                }
                log::warn!("[touch] disconnected, reconnecting in 2s…");
                thread::sleep(Duration::from_secs(2));
            }
        }))
    } else {
        None
    };

    if let Some(h) = pen_handle {
        h.join().unwrap();
    }
    if let Some(h) = touch_handle {
        h.join().unwrap();
    }

    Ok(())
}
