use std::env;
use std::path::PathBuf;
use std::process::Command;

/// Cross-compile the evgrab helper for ARM targets.
///
/// The helper is a tiny C program that exclusively grabs an evdev device
/// (EVIOCGRAB) and pipes raw events to stdout. It's uploaded to the
/// reMarkable at runtime and replaces the old kill -STOP approach.
///
/// Compiler lookup order:
///   1. Environment variable (ARMV7_CC / AARCH64_CC)
///   2. Common musl cross-compiler names
///   3. Common glibc cross-compiler names
fn main() {
    println!("cargo:rerun-if-changed=helper/evgrab.c");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    build_helper("armv7", &out_dir);
    build_helper("aarch64", &out_dir);
}

fn build_helper(arch: &str, out_dir: &PathBuf) {
    let cc = find_compiler(arch);
    let output = out_dir.join(format!("evgrab-{}", arch));

    eprintln!("Compiling evgrab for {} using {}", arch, cc);

    let status = Command::new(&cc)
        .args(["-static", "-Os", "-o"])
        .arg(&output)
        .arg("helper/evgrab.c")
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "Failed to run cross-compiler '{}': {}\n\
                 \n\
                 To build rm-pad you need C cross-compilers for ARM.\n\
                 \n\
                 On Ubuntu/Debian:\n\
                 \x20 sudo apt install gcc-arm-linux-gnueabihf gcc-aarch64-linux-gnu\n\
                 \n\
                 On Arch Linux (AUR):\n\
                 \x20 arm-linux-gnueabihf-gcc, aarch64-linux-gnu-gcc\n\
                 \n\
                 Or set {}_CC to point to your compiler.",
                cc,
                e,
                arch.to_uppercase()
            )
        });

    if !status.success() {
        panic!(
            "Cross-compiler '{}' failed with {}.\n\
             Check that the toolchain is correctly installed.",
            cc, status
        );
    }

    // Try to strip the binary for a smaller embed size.
    if let Some(strip) = find_tool("strip", arch) {
        let _ = Command::new(strip).arg(&output).status();
    }
}

fn find_compiler(arch: &str) -> String {
    // 1. Check environment variable override.
    let env_var = format!("{}_CC", arch.to_uppercase());
    if let Ok(cc) = env::var(&env_var) {
        return cc;
    }

    // 2. Try common cross-compiler names.
    let candidates = compiler_candidates(arch);

    for cc in &candidates {
        if command_exists(cc) {
            return cc.to_string();
        }
    }

    panic!(
        "No C cross-compiler found for {arch}.\n\
         \n\
         Tried: {candidates}\n\
         \n\
         Install one of the above, or set {env_var} to your compiler path.\n\
         \n\
         On Ubuntu/Debian:\n\
         \x20 sudo apt install gcc-arm-linux-gnueabihf gcc-aarch64-linux-gnu\n\
         \n\
         On Arch Linux (AUR):\n\
         \x20 arm-linux-gnueabihf-gcc, aarch64-linux-gnu-gcc",
        arch = arch,
        candidates = candidates.join(", "),
        env_var = env_var,
    );
}

fn compiler_candidates(arch: &str) -> Vec<&'static str> {
    match arch {
        "armv7" => vec![
            "arm-linux-musleabihf-gcc",
            "arm-linux-gnueabihf-gcc",
        ],
        "aarch64" => vec![
            "aarch64-linux-musl-gcc",
            "aarch64-linux-gnu-gcc",
        ],
        _ => panic!("Unknown target architecture: {}", arch),
    }
}

fn find_tool(tool: &str, arch: &str) -> Option<String> {
    let prefixes = match arch {
        "armv7" => &["arm-linux-musleabihf-", "arm-linux-gnueabihf-"][..],
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
