//! Persona avatars: display only, never part of the briefing.
//!
//! Every persona has a built-in avatar (original artwork embedded from
//! `assets/ui/avatars/`). `[[talk.personas]] icon` may point at the operator's
//! own raster image instead. The web layer serves whichever resolves, and
//! falls back to the built-in one for anything that does not: the page must
//! never break over a picture.
//!
//! Nothing here is read by [`crate::persona`]'s briefing functions
//! (`section_base` / `section_for`); the tests in `persona` pin that.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

/// Largest accepted icon: the config string, and the decoded or on-disk image.
pub const ICON_MAX_BYTES: usize = 256 * 1024;

/// Extensions a path icon may carry, with the mime each implies.
const EXTENSIONS: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

const DATA_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The embedded avatar for a persona id; a configured id with no built-in
/// gets the generic one.
pub fn builtin_svg(id: &str) -> &'static str {
    match id {
        "default" => include_str!("../assets/ui/avatars/default.svg"),
        "magi" => include_str!("../assets/ui/avatars/magi.svg"),
        "rei" => include_str!("../assets/ui/avatars/rei.svg"),
        "misato" => include_str!("../assets/ui/avatars/misato.svg"),
        "ritsuko" => include_str!("../assets/ui/avatars/ritsuko.svg"),
        "shinji" => include_str!("../assets/ui/avatars/shinji.svg"),
        "asuka" => include_str!("../assets/ui/avatars/asuka.svg"),
        "kaworu" => include_str!("../assets/ui/avatars/kaworu.svg"),
        _ => include_str!("../assets/ui/avatars/persona.svg"),
    }
}

/// The operator's fixed display avatar; independent of the persona roster.
pub fn operator_svg() -> &'static str {
    include_str!("../assets/ui/avatars/operator.svg")
}

/// The mime a `data:` icon declares and its payload, when the prefix is an
/// accepted one.
fn data_prefix(icon: &str) -> Option<(&'static str, &str)> {
    let rest = icon.strip_prefix("data:")?;
    let (head, payload) = rest.split_once(',')?;
    let mime = head.strip_suffix(";base64")?;
    let mime = DATA_MIMES.iter().find(|m| **m == mime)?;
    Some((mime, payload))
}

fn path_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    EXTENSIONS.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}

fn is_path_form(icon: &str) -> bool {
    icon.starts_with("~/") || Path::new(icon).is_absolute()
}

/// The static shape of an `icon` setting; nothing is read from disk. Relative
/// paths are refused on purpose: the repository layer is read from
/// `<remote>/<base>` and extracted elsewhere, so a relative path would point
/// at a place that does not exist.
pub fn validate_shape(icon: &str) -> Result<()> {
    let icon = icon.trim();
    if icon.is_empty() {
        bail!("`icon` is empty");
    }
    if icon.len() > ICON_MAX_BYTES {
        bail!("`icon` is longer than {} KiB", ICON_MAX_BYTES / 1024);
    }
    if icon.starts_with("data:") {
        if data_prefix(icon).is_none() {
            bail!("`icon` data URI must start `data:image/(png|jpeg|gif|webp);base64,`");
        }
        return Ok(());
    }
    if !is_path_form(icon) {
        bail!("`icon` must be an absolute path, a `~/` path or a data URI");
    }
    if Path::new(icon)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        bail!("`icon` path may not contain `..`");
    }
    if path_mime(Path::new(icon)).is_none() {
        bail!("`icon` path must end in .png, .jpg, .jpeg, .gif or .webp");
    }
    Ok(())
}

/// Read an icon: the mime it *declares* (extension or data URI) and its bytes.
/// The caller still checks the bytes' own magic number against that mime.
pub fn load(icon: &str) -> Result<(&'static str, Vec<u8>)> {
    let icon = icon.trim();
    validate_shape(icon)?;
    if let Some((mime, payload)) = data_prefix(icon) {
        let bytes = decode_base64(payload)?;
        if bytes.is_empty() {
            bail!("icon image is empty");
        }
        return Ok((mime, bytes));
    }
    let path = expand(icon)?;
    let mime = path_mime(&path).ok_or_else(|| anyhow::anyhow!("unsupported icon extension"))?;
    let meta = std::fs::metadata(&path)?;
    if !meta.is_file() {
        bail!("icon is not a regular file");
    }
    if meta.len() == 0 || meta.len() > ICON_MAX_BYTES as u64 {
        bail!("icon is empty or over {} KiB", ICON_MAX_BYTES / 1024);
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)?
        .take(ICON_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > ICON_MAX_BYTES {
        bail!("icon is over {} KiB", ICON_MAX_BYTES / 1024);
    }
    Ok((mime, bytes))
}

fn expand(icon: &str) -> Result<PathBuf> {
    match icon.strip_prefix("~/") {
        Some(rest) => {
            let Some(home) = dirs::home_dir() else {
                bail!("no home directory to expand `~/`");
            };
            Ok(home.join(rest))
        }
        None => Ok(PathBuf::from(icon)),
    }
}

/// A short stable token for the icon setting, so a changed setting changes the
/// avatar URL and the browser's copy goes stale (FNV-1a, as `notices::id_of`).
pub fn fingerprint(icon: Option<&str>) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in icon.unwrap_or("").trim().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Strict standard base64: padding required, no whitespace, size-capped.
fn decode_base64(s: &str) -> Result<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 4 != 0 {
        bail!("base64 length is not a multiple of 4");
    }
    let val = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(b.len() / 4 * 3);
    let chunks = b.len() / 4;
    for (i, chunk) in b.chunks(4).enumerate() {
        let pad = if i + 1 == chunks {
            chunk.iter().rev().take_while(|c| **c == b'=').count()
        } else {
            0
        };
        if pad > 2 {
            bail!("bad base64 padding");
        }
        let mut n: u32 = 0;
        for (j, c) in chunk.iter().enumerate() {
            let v = if j >= 4 - pad {
                0
            } else {
                match val(*c) {
                    Some(v) => v,
                    None => bail!("bad base64 character"),
                }
            };
            n = (n << 6) | v;
        }
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
        if out.len() > ICON_MAX_BYTES {
            bail!("icon image is over {} KiB", ICON_MAX_BYTES / 1024);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_has_its_own_svg() {
        let ids = [
            "default", "magi", "rei", "misato", "ritsuko", "shinji", "asuka", "kaworu",
        ];
        for id in ids {
            let svg = builtin_svg(id);
            assert!(svg.starts_with("<svg"), "{id}");
            assert_ne!(svg, builtin_svg("custom-id"), "{id}");
        }
        assert!(builtin_svg("custom-id").starts_with("<svg"));
    }

    #[test]
    fn shape_accepts_only_data_uris_and_absolute_raster_paths() {
        let abs = if cfg!(windows) {
            "C:/p/a.png"
        } else {
            "/p/a.png"
        };
        for ok in [abs, "~/a.JPG", "data:image/png;base64,AAAA"] {
            assert!(validate_shape(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "a.png",
            "./a.png",
            "~/../a.png",
            "/p/../a.png",
            "/p/a.svg",
            "/p/a",
            "data:image/svg+xml;base64,AAAA",
            "data:image/png,AAAA",
            "http://x/a.png",
        ] {
            assert!(validate_shape(bad).is_err(), "{bad}");
        }
        let long = format!("data:image/png;base64,{}", "A".repeat(ICON_MAX_BYTES));
        assert!(validate_shape(&long).is_err());
    }

    #[test]
    fn base64_is_strict() {
        assert_eq!(decode_base64("aGk=").unwrap(), b"hi");
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("YWJj").unwrap(), b"abc");
        assert!(decode_base64("aGk").is_err());
        assert!(decode_base64("a Gk=").is_err());
        assert!(decode_base64("a===").is_err());
    }

    #[test]
    fn fingerprint_follows_the_setting() {
        assert_eq!(fingerprint(None), fingerprint(Some(" ")));
        assert_ne!(fingerprint(Some("/a.png")), fingerprint(Some("/b.png")));
    }
}
