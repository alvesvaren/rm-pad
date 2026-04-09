use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Cross-compile evgrab; source lives in `rm-common/helper`.
fn main() {
    println!("cargo:rerun-if-changed=../rm-common/helper/evgrab.c");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let c_src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../rm-common/helper/evgrab.c");

    build_helper("armv7", &out_dir, &c_src);
    build_helper("aarch64", &out_dir, &c_src);
}

fn build_helper(arch: &str, out_dir: &PathBuf, c_src: &PathBuf) {
    let cc = find_compiler(arch);
    let output = out_dir.join(format!("evgrab-{}", arch));

    eprintln!("Compiling evgrab for {} using {}", arch, cc);

    let status = Command::new(&cc)
        .args(["-static", "-Os", "-o"])
        .arg(&output)
        .arg(c_src)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "Failed to run cross-compiler '{}': {}\n\
                 Install gcc-arm-linux-gnueabihf and gcc-aarch64-linux-gnu, or set {}_CC.",
                cc,
                e,
                arch.to_uppercase()
            )
        });

    if !status.success() {
        panic!("Cross-compiler '{}' failed with {}.", cc, status);
    }

    if let Some(strip) = find_tool("strip", arch) {
        let _ = Command::new(strip).arg(&output).status();
    }
}

fn find_compiler(arch: &str) -> String {
    let env_var = format!("{}_CC", arch.to_uppercase());
    if let Ok(cc) = env::var(&env_var) {
        return cc;
    }

    for cc in compiler_candidates(arch) {
        if command_exists(cc) {
            return cc.to_string();
        }
    }

    panic!(
        "No C cross-compiler found for {arch}. Tried: {}. Set {env_var}.",
        compiler_candidates(arch).join(", "),
        env_var = env_var,
    );
}

fn compiler_candidates(arch: &str) -> Vec<&'static str> {
    match arch {
        "armv7" => vec![
            "arm-none-linux-gnueabihf-gcc",
            "arm-linux-musleabihf-gcc",
            "arm-linux-gnueabihf-gcc",
        ],
        "aarch64" => vec!["aarch64-linux-musl-gcc", "aarch64-linux-gnu-gcc"],
        _ => panic!("Unknown arch {}", arch),
    }
}

fn find_tool(tool: &str, arch: &str) -> Option<String> {
    let prefixes = match arch {
        "armv7" => &["arm-none-linux-gnueabihf-", "arm-linux-musleabihf-", "arm-linux-gnueabihf-"][..],
        "aarch64" => &["aarch64-linux-musl-", "aarch64-linux-gnu-"][..],
        _ => return None,
    };

    for prefix in prefixes {
        let name = format!("{}{}", prefix, tool);
        if command_exists(&name) {
            return Some(name);
        }
    }
    None
}

fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}
