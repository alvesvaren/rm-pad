mod config;
mod dump;
mod event;
mod palm;
mod pen;
mod ssh;
mod touch;

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const DEFAULT_PALM_GRACE_MS: u64 = 500;

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
    let no_palm_rejection = args.iter().any(|a| a == "--no-palm-rejection");
    let palm_grace_ms: u64 = args
        .iter()
        .find_map(|a| a.strip_prefix("--palm-grace-ms="))
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PALM_GRACE_MS);
    let run_pen = !touch_only;
    let run_touch = !pen_only;

    let palm_state: Option<palm::SharedPalmState> =
        if run_pen && run_touch && !no_palm_rejection {
            Some(Arc::new(std::sync::Mutex::new(palm::PalmState::new())))
        } else {
            None
        };

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!(
        "rm-mouse starting (host={}, pen={}, touch={}, palm_rejection={})",
        config::HOST,
        if run_pen { config::PEN_DEVICE } else { "off" },
        if run_touch { config::TOUCH_DEVICE } else { "off" },
        if palm_state.is_some() {
            format!("on (grace {}ms)", palm_grace_ms)
        } else {
            "off".into()
        }
    );

    let key_path = Path::new(config::KEY_PATH);

    if !run_pen && !run_touch {
        eprintln!(
            "Usage: {} [--pen-only] [--touch-only] [--no-palm-rejection] [--palm-grace-ms=N]",
            args.get(0).unwrap_or(&"rm-mouse".into())
        );
        eprintln!("  Default: run both pen and touch; palm rejection on with 500ms grace.");
        std::process::exit(1);
    }

    let pen_handle = if run_pen {
        let key_pen = key_path.to_path_buf();
        let palm_pen = palm_state.clone();
        Some(thread::spawn(move || {
            loop {
                log::info!("[pen] thread starting…");
                if let Err(e) = pen::run(&key_pen, palm_pen.clone()) {
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
        let palm_touch = palm_state.clone();
        let grace = palm_grace_ms;
        Some(thread::spawn(move || {
            loop {
                log::info!("[touch] thread starting…");
                if let Err(e) = touch::run(&key_touch, palm_touch.clone(), grace) {
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
