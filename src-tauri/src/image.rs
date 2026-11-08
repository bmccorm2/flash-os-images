//! Image kind detection and size info.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ImageKind {
    Iso,
    Img,
    ImgXz,
}

impl ImageKind {
    /// Picks the kind from the lowercase file name suffix.
    pub fn from_name(name: &str) -> Option<Self> {
        let lower = name.to_lowercase();
        let stem_ok = |suffix: &str| lower.len() > suffix.len() && lower.ends_with(suffix);
        if stem_ok(".img.xz") {
            Some(Self::ImgXz)
        } else if stem_ok(".iso") {
            Some(Self::Iso)
        } else if stem_ok(".img") {
            Some(Self::Img)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInfo {
    pub path: String,
    pub name: String,
    pub kind: ImageKind,
    pub file_size: u64,
    /// False for `.img.xz`: the uncompressed size isn't known up front.
    pub size_known: bool,
}

impl ImageInfo {
    pub fn from_path(path: &Path) -> Result<Self> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(kind) = ImageKind::from_name(&name) else {
            bail!("Unsupported file type. Use an .iso, .img, or .img.xz file.");
        };
        let meta = std::fs::metadata(path).with_context(|| format!("Can't read {name}"))?;
        if !meta.is_file() {
            bail!("{name} is not a file");
        }
        Ok(Self {
            path: path.to_string_lossy().into_owned(),
            name,
            kind,
            file_size: meta.len(),
            size_known: kind != ImageKind::ImgXz,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_rules() {
        assert_eq!(ImageKind::from_name("ubuntu.iso"), Some(ImageKind::Iso));
        assert_eq!(ImageKind::from_name("UBUNTU.ISO"), Some(ImageKind::Iso));
        assert_eq!(ImageKind::from_name("pi.img"), Some(ImageKind::Img));
        assert_eq!(ImageKind::from_name("pi.IMG.XZ"), Some(ImageKind::ImgXz));
        assert_eq!(ImageKind::from_name("a b.img.xz"), Some(ImageKind::ImgXz));
        for bad in [
            "foo.xz",
            "foo.iso.xz",
            "foo.zip",
            "foo.img.gz",
            "foo.dmg",
            "foo",
            ".img",
            ".iso",
            ".img.xz",
            "foo.img.xz.bak",
            "isofile",
        ] {
            assert_eq!(ImageKind::from_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn info_from_path() {
        let dir = std::env::temp_dir().join(format!("flashos-img-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.img.xz");
        std::fs::write(&p, b"12345").unwrap();
        let i = ImageInfo::from_path(&p).unwrap();
        assert_eq!(i.kind, ImageKind::ImgXz);
        assert_eq!(i.file_size, 5);
        assert!(!i.size_known);
        assert!(ImageInfo::from_path(&dir.join("missing.iso")).is_err());
        assert!(ImageInfo::from_path(&dir.join("x.txt")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
