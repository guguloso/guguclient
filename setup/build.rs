use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::write::DeflateEncoder;
use flate2::Compression;

fn find_rc_exe() -> Option<PathBuf> {
    if let Ok(output) = Command::new("where").arg("rc.exe").output() {
        if output.status.success() {
            let line = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(str::trim)
                .unwrap_or("")
                .to_owned();
            if !line.is_empty() {
                return Some(PathBuf::from(line));
            }
        }
    }
    let kits_bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if let Ok(entries) = fs::read_dir(kits_bin) {
        let mut versions: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        versions.sort();
        for dir in versions.into_iter().rev() {
            let candidate = dir.join("x64").join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let client_path = env::var_os("GUGU_CLIENT_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("../src-tauri/target/release/GuguClient.exe"));

    println!("cargo:rerun-if-env-changed=GUGU_CLIENT_EXE");
    println!("cargo:rerun-if-changed={}", client_path.display());

    let compressed_path = out_dir.join("GuguClient.deflate");
    if client_path.is_file() {
        let mut raw = Vec::new();
        File::open(&client_path)
            .expect("open GuguClient.exe")
            .read_to_end(&mut raw)
            .expect("read GuguClient.exe");
        let out = File::create(&compressed_path).expect("create GuguClient.deflate");
        let mut encoder = DeflateEncoder::new(out, Compression::best());
        encoder.write_all(&raw).expect("compress GuguClient.exe");
        encoder.finish().expect("finish compression");
        println!("cargo:rustc-env=GUGU_PAYLOAD_RAW_SIZE={}", raw.len());
    } else {
        // Allows `cargo check` before `GuguClient.exe` is built.
        fs::write(&compressed_path, []).expect("write placeholder payload");
        println!("cargo:rustc-env=GUGU_PAYLOAD_RAW_SIZE=0");
        println!(
            "cargo:warning=GuguClient.exe not found at {}; built placeholder payload",
            client_path.display()
        );
    }

    let icon_path = manifest_dir.join("../src-tauri/icons/icon.ico");
    let app_manifest = manifest_dir.join("app.manifest");
    println!("cargo:rerun-if-changed={}", icon_path.display());
    println!("cargo:rerun-if-changed={}", app_manifest.display());

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        if let Some(rc) = find_rc_exe() {
            let rc_file = out_dir.join("setup.rc");
            let res_file = out_dir.join("setup.res");
            let rc_content = format!(
                "1 ICON \"{}\"\n1 24 \"{}\"\n",
                icon_path.display().to_string().replace('\\', "\\\\"),
                app_manifest.display().to_string().replace('\\', "\\\\")
            );
            fs::write(&rc_file, rc_content).expect("write setup.rc");
            let status = Command::new(rc)
                .args(["/nologo", "/fo"])
                .arg(&res_file)
                .arg(&rc_file)
                .status();
            if status.is_ok_and(|s| s.success()) {
                println!("cargo:rustc-link-arg-bins={}", res_file.display());
            }
        }
    }
}
