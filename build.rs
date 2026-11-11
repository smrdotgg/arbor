//! Windows only: gives `arbor.exe` its icon and version information, with no resource compiler
//! and no extra crate. It writes a compiled resource file (`.res`: the icon images, their icon
//! group and a `VS_VERSIONINFO` with the version from Cargo.toml) into `OUT_DIR` and hands it to
//! MSVC's linker, which takes `.res` files as input. Other targets: nothing.

use std::path::{Path, PathBuf};

/// The icon sizes in `assets/icon/png` that go into the executable (PNG images, which Windows
/// reads at every size since Vista).
const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=assets/icon/png");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" {
        refuse_crlf_shaders();
    }
    if os != "windows" || env != "msvc" {
        return;
    }
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let images: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&n| {
            let p = root.join(format!("assets/icon/png/arbor-{n}.png"));
            (n, std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
        })
        .collect();
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let res = resource_file(&images, &version);
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("arbor.res");
    std::fs::write(&out, res).expect("writing arbor.res");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

/// Rune's WGSL as Rune embeds it. On Windows Rune draws with a precompiled DXBC copy of its uber
/// shader only while this text hashes exactly as committed; a checkout that turned its line
/// endings into CRLF (Git for Windows' `core.autocrlf true`, the GitHub runner's default) makes
/// every launch compile it with FXC instead, hundreds of milliseconds before the first frame,
/// with no message (BENCH.md, 2026-11-12). So such a build stops here.
const RUNE_WGSL: [&str; 2] = ["vendor/rune/crates/rune/src/shader.wgsl", "vendor/rune/crates/rune/src/clip.wgsl"];

fn refuse_crlf_shaders() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    for rel in RUNE_WGSL {
        let path = root.join(rel);
        println!("cargo:rerun-if-changed={}", path.display());
        // A layout without vendor/rune (Rune from elsewhere) can't be checked here.
        let Ok(text) = std::fs::read(&path) else { continue };
        assert!(
            !text.contains(&b'\r'),
            "{rel} has CRLF line endings, so Rune would compile its uber shader at every launch instead of \
             loading the precompiled one. Check Rune out again without converting line endings: \
             `git -C vendor/rune config core.autocrlf false`, then `git -C vendor/rune checkout -- .` \
             (or `git config --global core.autocrlf false` before cloning)."
        );
    }
}

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
/// English (United States), and Unicode (UTF-16) for the version strings.
const LANG: u16 = 0x0409;
const CODEPAGE: u16 = 1200;

/// A `.res` file: the empty resource every one starts with, then the icons, the icon group
/// (id 1, the one Explorer shows) and the version information.
fn resource_file(images: &[(u32, Vec<u8>)], version: &str) -> Vec<u8> {
    let mut out = Vec::new();
    resource(&mut out, 0, 0, 0, 0, &[]);
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(images.len() as u16).to_le_bytes());
    for (i, (size, png)) in images.iter().enumerate() {
        let id = i as u16 + 1;
        // MOVEABLE | DISCARDABLE, as rc writes icons.
        resource(&mut out, RT_ICON, id, 0x1010, LANG, png);
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        group.extend_from_slice(&[dim, dim, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes());
        group.extend_from_slice(&32u16.to_le_bytes());
        group.extend_from_slice(&(png.len() as u32).to_le_bytes());
        group.extend_from_slice(&id.to_le_bytes());
    }
    resource(&mut out, RT_GROUP_ICON, 1, 0x1030, LANG, &group);
    resource(&mut out, RT_VERSION, 1, 0x0030, LANG, &version_info(version));
    out
}

/// One resource: its header (ordinal type and name) and its data, padded to four bytes.
fn resource(out: &mut Vec<u8>, kind: u16, name: u16, flags: u16, lang: u16, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    out.extend_from_slice(&[0xFF, 0xFF]);
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&[0xFF, 0xFF]);
    out.extend_from_slice(&name.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&lang.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // Version
    out.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    out.extend_from_slice(data);
    pad(out);
}

fn pad(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

/// One node of a `VS_VERSIONINFO` tree: length, value length, type (1 text, 0 binary), its
/// key, its value and its children, each aligned to four bytes.
fn node(key: &str, value: &[u8], value_len: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut v = vec![0, 0];
    v.extend_from_slice(&value_len.to_le_bytes());
    v.extend_from_slice(&u16::from(text).to_le_bytes());
    v.extend_from_slice(&utf16z(key));
    pad(&mut v);
    v.extend_from_slice(value);
    for c in children {
        pad(&mut v);
        v.extend_from_slice(c);
    }
    let len = v.len() as u16;
    v[..2].copy_from_slice(&len.to_le_bytes());
    v
}

/// `VS_VERSIONINFO`: the fixed part (file and product version as numbers) and the strings
/// Explorer's Details tab shows.
fn version_info(version: &str) -> Vec<u8> {
    let mut n = version.split(['.', '-', '+']).map(|p| p.parse::<u16>().unwrap_or(0));
    let (a, b, c) = (n.next().unwrap_or(0), n.next().unwrap_or(0), n.next().unwrap_or(0));
    let ms = (u32::from(a) << 16) | u32::from(b);
    let ls = u32::from(c) << 16;
    let mut fixed = Vec::new();
    for d in [0xFEEF_04BDu32, 0x0001_0000, ms, ls, ms, ls, 0x3F, 0, 0x0004_0004, 1, 0, 0, 0] {
        fixed.extend_from_slice(&d.to_le_bytes());
    }
    let strings: Vec<Vec<u8>> = [
        ("CompanyName", "smr"),
        ("FileDescription", "Arbor"),
        ("FileVersion", version),
        ("InternalName", "arbor"),
        ("LegalCopyright", "MIT License"),
        ("OriginalFilename", "arbor.exe"),
        ("ProductName", "Arbor"),
        ("ProductVersion", version),
    ]
    .iter()
    .map(|(k, v)| {
        let value = utf16z(v);
        node(k, &value, (value.len() / 2) as u16, true, &[])
    })
    .collect();
    let table = node(&format!("{LANG:04X}{CODEPAGE:04X}"), &[], 0, true, &strings);
    let string_info = node("StringFileInfo", &[], 0, true, &[table]);
    let mut translation = LANG.to_le_bytes().to_vec();
    translation.extend_from_slice(&CODEPAGE.to_le_bytes());
    let var = node("Translation", &translation, 4, false, &[]);
    let var_info = node("VarFileInfo", &[], 0, true, &[var]);
    node("VS_VERSION_INFO", &fixed, fixed.len() as u16, false, &[string_info, var_info])
}
