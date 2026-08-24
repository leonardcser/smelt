mod generated;

use crate::style::{Color, Style};
use crate::theme::{intern_anonymous_style, HlGroup};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileIconOptions {
    pub enabled: bool,
    pub colors: bool,
    pub light: bool,
    pub base_dir: Option<PathBuf>,
}

impl FileIconOptions {
    pub fn new(enabled: bool, colors: bool, light: bool, base_dir: Option<PathBuf>) -> Self {
        Self {
            enabled,
            colors,
            light,
            base_dir,
        }
    }

    pub fn cache_hash(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }

    pub fn dynamic_retained_bytes(&self) -> usize {
        self.base_dir.as_ref().map_or(0, PathBuf::capacity)
    }

    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.dynamic_retained_bytes())
    }
}

impl Default for FileIconOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            colors: true,
            light: false,
            base_dir: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIcon {
    pub icon: &'static str,
    pub group: Option<HlGroup>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IconEntry {
    key: &'static str,
    icon: &'static str,
    #[allow(dead_code)]
    name: &'static str,
    dark: (u8, u8, u8),
    light: (u8, u8, u8),
}

pub fn lookup_path(path: &Path, options: &FileIconOptions) -> Option<FileIcon> {
    if !options.enabled {
        return None;
    }
    let file_name = path.file_name()?.to_str()?;
    let entry = lookup(file_name).unwrap_or(&generated::DEFAULT_ICON);
    Some(FileIcon {
        icon: entry.icon,
        group: icon_group(entry, options),
    })
}

fn icon_group(entry: &IconEntry, options: &FileIconOptions) -> Option<HlGroup> {
    if !options.colors {
        return None;
    }
    let color = if options.light {
        entry.light
    } else {
        entry.dark
    };
    Some(intern_anonymous_style(Style {
        fg: Some(Color::Rgb {
            r: color.0,
            g: color.1,
            b: color.2,
        }),
        ..Default::default()
    }))
}

fn lookup(file_name: &str) -> Option<&'static IconEntry> {
    let lower_name = file_name.to_ascii_lowercase();
    lookup_in(generated::BY_FILENAME, &lower_name)
        .or_else(|| lookup_compound_extension(file_name))
        .or_else(|| lookup_compound_extension(&lower_name))
        .or_else(|| lookup_extension(file_name))
}

fn lookup_extension(file_name: &str) -> Option<&'static IconEntry> {
    let ext = Path::new(file_name).extension()?.to_str()?;
    lookup_in(generated::BY_EXTENSION, ext)
        .or_else(|| lookup_in(generated::BY_EXTENSION, &ext.to_ascii_lowercase()))
}

fn lookup_compound_extension(file_name: &str) -> Option<&'static IconEntry> {
    let (_, mut ext) = file_name.split_once('.')?;
    loop {
        if let Some(icon) = lookup_in(generated::BY_EXTENSION, ext) {
            return Some(icon);
        }
        let (_, rest) = ext.split_once('.')?;
        ext = rest;
    }
}

fn lookup_in(table: &'static [IconEntry], key: &str) -> Option<&'static IconEntry> {
    table
        .binary_search_by(|entry| entry.key.cmp(key))
        .ok()
        .map(|index| &table[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(enabled: bool) -> FileIconOptions {
        FileIconOptions::new(enabled, false, false, None)
    }

    #[test]
    fn disabled_lookup_returns_none() {
        assert_eq!(lookup_path(Path::new("Cargo.toml"), &options(false)), None);
    }

    #[test]
    fn lookup_matches_filename_before_extension() {
        let icon = lookup_path(Path::new("Makefile"), &options(true)).unwrap();
        assert!(!icon.icon.is_empty());
    }

    #[test]
    fn lookup_matches_compound_extension() {
        let icon = lookup_path(Path::new("view.blade.php"), &options(true)).unwrap();
        assert_eq!(icon.icon, "");
    }
}
