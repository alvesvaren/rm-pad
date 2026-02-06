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

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    log::info!("rm-mouse starting (host={}, pen={}, touch={})", config::HOST, config::PEN_DEVICE, config::TOUCH_DEVICE);

    let key_path = Path::new(config::KEY_PATH);
    let key_pen = key_path.to_path_buf();
    let key_touch = key_path.to_path_buf();

    let pen_handle = thread::spawn(move || {
        loop {
            log::info!("[pen] thread starting…");
            if let Err(e) = pen::run(&key_pen) {
                log::error!("[pen] {}", e);
            }
            log::warn!("[pen] disconnected, reconnecting in 2s…");
            thread::sleep(Duration::from_secs(2));
        }
    });
    let touch_handle = thread::spawn(move || {
        loop {
            log::info!("[touch] thread starting…");
            if let Err(e) = touch::run(&key_touch) {
                log::error!("[touch] {}", e);
            }
            log::warn!("[touch] disconnected, reconnecting in 2s…");
            thread::sleep(Duration::from_secs(2));
        }
    });

    pen_handle.join().unwrap();
    touch_handle.join().unwrap();
    Ok(())
}
