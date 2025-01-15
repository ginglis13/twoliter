use flate2::{read::GzDecoder, write::GzEncoder};
use std::fs::File;
use std::io::{self, prelude::*};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};
use tar::Archive;

const CRANE_VERSION: &str = "0.20.1";

const REQUIRED_TOOLS: &[&str] = &["patch", "go"];

fn main() {
    let script_dir = env::current_dir().unwrap();
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    println!("cargo::rerun-if-changed=../build-cache-fetch");
    println!("cargo::rerun-if-changed=hashes/crane");
    println!("cargo::rerun-if-changed=patches");

    ensure_required_tools_installed();

    // Download and checksum-verify crane
    env::set_current_dir(&out_dir).expect("Failed to set current directory");
    Command::new(script_dir.join("../build-cache-fetch"))
        .arg(script_dir.join("hashes/crane"))
        .status()
        .expect("Failed to execute build-cache-fetch");

    // extract crane sources
    let crane_archive = out_dir.join(format!("go-containerregistry-v{CRANE_VERSION}.tar.gz"));
    let crane_tgz = File::open(&crane_archive).expect("Failed to open crane archive");
    let mut tar_archive = Archive::new(GzDecoder::new(crane_tgz));

    let crane_output_dir = out_dir.join(format!("go-containerregistry-v{CRANE_VERSION}"));
    tar_archive
        .unpack(&crane_output_dir)
        .expect("Failed to extract crane sources");

    // Perform any local modifications
    let crane_source_dir = crane_output_dir.join(format!("go-containerregistry-{CRANE_VERSION}"));
    apply_source_patches(&crane_source_dir, script_dir.join("patches"));

    // build krane
    let build_output_loc = out_dir.join("krane");
    Command::new("go")
        .arg("build")
        .env("GOOS", get_goos())
        .env("GOARCH", get_goarch())
        .arg("-o")
        .arg(&build_output_loc)
        .current_dir(crane_source_dir.join("cmd/krane"))
        .status()
        .expect("Failed to build crane");

    // compress krane
    let krane_gz_path = out_dir.join("krane.gz");
    let compressed_output_file =
        File::create(&krane_gz_path).expect("Failed to crate krane.gz file");

    let krane_binary = File::open(&build_output_loc).expect("Failed to open krane binary");
    let mut reader = io::BufReader::new(&krane_binary);
    let mut encoder = GzEncoder::new(&compressed_output_file, flate2::Compression::best());

    let mut buffer = Vec::with_capacity(
        krane_binary
            .metadata()
            .expect("Failed to get krane binary metadata")
            .len() as usize,
    );
    reader
        .read_to_end(&mut buffer)
        .expect("Failed to read krane binary");
    encoder
        .write_all(&buffer)
        .expect("Failed to write compressed krane binary");
    encoder
        .finish()
        .expect("Failed to finish writing compressed krane binary");

    println!("cargo::rustc-env=KRANE_GZ_PATH={}", krane_gz_path.display());
}

fn ensure_required_tools_installed() {
    for tool in REQUIRED_TOOLS {
        which::which(tool)
            .unwrap_or_else(|_| panic!("Must have the `{tool}` utility installed in PATH"));
    }
}

fn apply_source_patches(source_path: impl AsRef<Path>, patch_dir: impl AsRef<Path>) {
    let source_path = source_path.as_ref();
    let patch_dir = patch_dir.as_ref();

    let mut patches = fs::read_dir(patch_dir)
        .expect("Failed to read patch directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|ext| ext == "patch").unwrap_or(false))
        .collect::<Vec<_>>();
    patches.sort();

    for patch in patches {
        println!("Executing `patch -p1 -i '{}'`", patch.display());

        let patch_status = Command::new("patch")
            .current_dir(source_path)
            .arg("-p1")
            .arg("-i")
            .arg(patch.as_os_str())
            .status()
            .expect("Failed to execute patch command");

        if !patch_status.success() {
            panic!("Failed to apply patch '{}'", patch.display());
        }
    }
}

fn get_goos() -> &'static str {
    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("Failed to read CARGO_CFG_TARGET_OS");
    match target_os.as_str() {
        "linux" => "linux",
        "windows" => "windows",
        "macos" => "darwin",
        // Add more mappings as needed
        other => panic!("Unsupported target OS: {}", other),
    }
}

fn get_goarch() -> &'static str {
    let target_arch =
        env::var("CARGO_CFG_TARGET_ARCH").expect("Failed to read CARGO_CFG_TARGET_ARCH");

    match target_arch.as_str() {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        "wasm32" => "wasm",
        // Add more mappings as needed
        other => panic!("Unsupported target architecture: {}", other),
    }
}
