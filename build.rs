//! Embed a deterministic build identity covering compiled inputs and settings.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    self_check_sha256();

    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));

    for key in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTUP_TOOLCHAIN",
        "RUSTC",
    ] {
        println!("cargo::rerun-if-env-changed={key}");
    }

    let mut hasher = Sha256::new();
    hasher.update_tagged(b"protocol", b"magi-build-id-v1");

    for key in [
        "CARGO_PKG_NAME",
        "CARGO_PKG_VERSION",
        "PROFILE",
        "TARGET",
        "HOST",
        "OPT_LEVEL",
        "DEBUG",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
    ] {
        let value = env::var(key).unwrap_or_default();
        hasher.update_tagged(key.as_bytes(), value.as_bytes());
    }

    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let rustc_out = Command::new(&rustc)
        .args(["--version", "--verbose"])
        .output()
        .expect("query rustc --version --verbose");
    assert!(
        rustc_out.status.success(),
        "rustc --version --verbose failed: {}",
        String::from_utf8_lossy(&rustc_out.stderr)
    );
    hasher.update_tagged(b"rustc-version-verbose", &rustc_out.stdout);

    let mut cfg_and_features = BTreeMap::new();
    for (key, value) in env::vars() {
        if key.starts_with("CARGO_CFG_") || key.starts_with("CARGO_FEATURE_") {
            cfg_and_features.insert(key, value);
        }
    }
    for (key, value) in cfg_and_features {
        hasher.update_tagged(key.as_bytes(), value.as_bytes());
    }

    hash_required_file(&mut hasher, &manifest_dir, "build.rs");
    hash_required_file(&mut hasher, &manifest_dir, "Cargo.toml");
    hash_optional_file(&mut hasher, &manifest_dir, "Cargo.lock");

    hash_tree(&mut hasher, &manifest_dir, "src");
    hash_tree(&mut hasher, &manifest_dir, "assets");

    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let digest = hasher.finalize_hex();
    println!("cargo::rustc-env=MAGI_BUILD_ID={version}-{digest}");
}

fn hash_required_file(hasher: &mut Sha256, root: &Path, rel: &str) {
    println!("cargo::rerun-if-changed={rel}");
    let bytes = fs::read(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
    hasher.update_tagged(b"file", rel.as_bytes());
    hasher.update_tagged(b"bytes", &bytes);
}

fn hash_optional_file(hasher: &mut Sha256, root: &Path, rel: &str) {
    println!("cargo::rerun-if-changed={rel}");
    let path = root.join(rel);
    match fs::read(&path) {
        Ok(bytes) => {
            hasher.update_tagged(b"optional-file-present", rel.as_bytes());
            hasher.update_tagged(b"bytes", &bytes);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            hasher.update_tagged(b"optional-file-absent", rel.as_bytes());
        }
        Err(e) => panic!("read {rel}: {e}"),
    }
}

fn hash_tree(hasher: &mut Sha256, root: &Path, dir: &str) {
    let start = root.join(dir);
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    collect_tree(&start, dir, &mut dirs, &mut files);
    dirs.sort();
    files.sort_by(|a, b| a.0.cmp(&b.0));

    for rel_dir in dirs {
        println!("cargo::rerun-if-changed={rel_dir}");
    }
    for (rel_file, full_path) in files {
        println!("cargo::rerun-if-changed={rel_file}");
        let bytes = fs::read(&full_path).unwrap_or_else(|e| panic!("read {rel_file}: {e}"));
        hasher.update_tagged(b"file", rel_file.as_bytes());
        hasher.update_tagged(b"bytes", &bytes);
    }
}

fn collect_tree(
    dir: &Path,
    rel_prefix: &str,
    dirs: &mut Vec<String>,
    files: &mut Vec<(String, PathBuf)>,
) {
    dirs.push(rel_prefix.to_owned());
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {rel_prefix}: {e}"))
        .map(|entry| entry.unwrap_or_else(|e| panic!("dir entry in {rel_prefix}: {e}")))
        .collect();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .unwrap_or_else(|raw| panic!("non-UTF-8 path entry in {rel_prefix}: {raw:?}"));
        let rel = format!("{rel_prefix}/{name}");
        let path = entry.path();
        let meta = fs::metadata(&path).unwrap_or_else(|e| panic!("metadata {rel}: {e}"));
        if meta.is_dir() {
            collect_tree(&path, &rel, dirs, files);
        } else if meta.is_file() {
            files.push((rel, path));
        }
    }
}

fn self_check_sha256() {
    let mut h = Sha256::new();
    h.update(b"abc");
    assert_eq!(
        h.finalize_hex(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

/// Minimal pure-Rust FIPS 180-4 SHA-256 so `build.rs` needs no build-dependencies.
struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffered: usize,
    total_bits: u64,
}

impl Sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0; 64],
            buffered: 0,
            total_bits: 0,
        }
    }

    fn update_tagged(&mut self, tag: &[u8], data: &[u8]) {
        self.update(&(tag.len() as u64).to_le_bytes());
        self.update(tag);
        self.update(&(data.len() as u64).to_le_bytes());
        self.update(data);
    }

    fn update(&mut self, mut input: &[u8]) {
        self.total_bits = self.total_bits.wrapping_add((input.len() as u64) * 8);
        if self.buffered > 0 {
            let take = (64 - self.buffered).min(input.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&input[..take]);
            self.buffered += take;
            input = &input[take..];
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
        while input.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&input[..64]);
            self.compress(&block);
            input = &input[64..];
        }
        if !input.is_empty() {
            self.buffer[..input.len()].copy_from_slice(input);
            self.buffered = input.len();
        }
    }

    fn finalize_hex(mut self) -> String {
        let total_bits = self.total_bits;
        self.buffer[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > 56 {
            self.buffer[self.buffered..64].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffered = 0;
        }
        self.buffer[self.buffered..56].fill(0);
        self.buffer[56..64].copy_from_slice(&total_bits.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);

        let mut out = String::with_capacity(64);
        for word in self.state {
            use std::fmt::Write as _;
            let _ = write!(out, "{word:08x}");
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for (&k, &word) in Self::K.iter().zip(w.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k)
                .wrapping_add(word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, delta) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(delta);
        }
    }
}
