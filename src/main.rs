mod config;
mod dump;
mod event;
mod palm;
mod pen;
mod ssh;
mod touch;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(|s| s.as_str()) == Some("dump") {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
        let cfg = config::load();
        match args.get(2).map(|s| s.as_str()) {
            Some("touch") => return dump::run_dump_touch(&cfg),
            Some("pen") => return dump::run_dump_pen(&cfg),
            _ => {
                eprintln!("Usage: {} dump <touch|pen>", args.get(0).unwrap_or(&"rm-mouse".into()));
                eprintln!("  Streams and prints raw input events from the reMarkable for debugging.");
                std::process::exit(1);
            }
        }
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut cfg = config::load();

    // CLI overrides (same flags as before)
    if args.iter().any(|a| a == "--touch-only") {
        cfg.touch_only = true;
    }
    if args.iter().any(|a| a == "--pen-only") {
        cfg.pen_only = true;
    }
    if args.iter().any(|a| a == "--stop-ui") {
        cfg.stop_ui = true;
    }
    if args.iter().any(|a| a == "--no-stop-ui") {
        cfg.stop_ui = false;
    }
    if args.iter().any(|a| a == "--no-palm-rejection") {
        cfg.no_palm_rejection = true;
    }
    if let Some(s) = args.iter().find_map(|a| a.strip_prefix("--palm-grace-ms=")) {
        if let Ok(n) = s.parse::<u64>() {
            cfg.palm_grace_ms = n;
        }
    }

    let stop_ui = cfg.stop_ui;
    let run_pen = !cfg.touch_only;
    let run_touch = !cfg.pen_only;

    let palm_state: Option<palm::SharedPalmState> =
        if run_pen && run_touch && !cfg.no_palm_rejection {
            Some(Arc::new(std::sync::Mutex::new(palm::PalmState::new())))
        } else {
            None
        };

    log::info!(
        "rm-mouse starting (host={}, pen={}, touch={}, palm_rejection={}, stop_ui={})",
        cfg.host,
        if run_pen { cfg.pen_device.as_str() } else { "off" },
        if run_touch { cfg.touch_device.as_str() } else { "off" },
        if palm_state.is_some() {
            format!("on (grace {}ms)", cfg.palm_grace_ms)
        } else {
            "off".into()
        },
        stop_ui
    );

    if !run_pen && !run_touch {
        eprintln!(
            "Usage: {} [--pen-only] [--touch-only] [--stop-ui] [--no-stop-ui] [--no-palm-rejection] [--palm-grace-ms=N]",
            args.get(0).unwrap_or(&"rm-mouse".into())
        );
        eprintln!("  Config file: RMMOUSE_CONFIG, ./rm-mouse.toml, or ~/.config/rm-mouse/config.toml");
        std::process::exit(1);
    }

    let config = Arc::new(cfg);
    let pause_refcount = if stop_ui && (run_pen || run_touch) {
        Some(Arc::new(std::sync::atomic::AtomicUsize::new(0)))
    } else {
        None
    };

    let pen_handle = if run_pen {
        let config_pen = config.clone();
        let palm_pen = palm_state.clone();
        let pause_ref = pause_refcount.clone();
        Some(thread::spawn(move || {
            loop {
                log::info!("[pen] thread starting…");
                if let Err(e) = pen::run(&config_pen, palm_pen.clone(), pause_ref.clone()) {
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
        let config_touch = config.clone();
        let palm_touch = palm_state.clone();
        let grace = config.palm_grace_ms;
        let pause_ref = pause_refcount.clone();
        Some(thread::spawn(move || {
            loop {
                log::info!("[touch] thread starting…");
                if let Err(e) = touch::run(&config_touch, palm_touch.clone(), grace, pause_ref.clone()) {
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
