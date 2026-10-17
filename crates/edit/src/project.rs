//! The footage folder an edit works on: its takes and the `edit/` folder outputs go to.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{EditError, Result};
use crate::probe::{MediaInfo, probe, probe_image};

/// File extensions of the takes (lowercase): video, then audio. Rushes' footage list
/// and the pipeline's prompt use the same list.
pub const FOOTAGE_EXTENSIONS: [&str; 23] = [
    "mp4", "mov", "m4v", "mkv", "webm", "avi", "mts", "m2ts", "mxf", "mpg", "mpeg", "wmv", "3gp", "wav", "mp3", "m4a",
    "aac", "flac", "ogg", "opus", "aif", "aiff", "wma",
];

/// Still images a cut can show (over the speech, or held on their own). Takes too, but
/// not footage in Rushes' list.
pub const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "bmp"];

pub const EDIT_DIR: &str = "edit";
/// `edit/takes.json`: the id every file got, so adding a file never renames the others.
const TAKE_IDS: &str = "takes.json";

/// What kind of take a file name is, by its extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Footage,
    Image,
}

pub fn media_kind(name: &str) -> Option<MediaKind> {
    let ext = Path::new(name).extension().and_then(OsStr::to_str)?.to_ascii_lowercase();
    if FOOTAGE_EXTENSIONS.contains(&ext.as_str()) {
        Some(MediaKind::Footage)
    } else if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        Some(MediaKind::Image)
    } else {
        None
    }
}

/// One source file. Takes are named `A`, `B`, ... in file-name order when first seen and
/// keep their name after that ([`TAKE_IDS`]); phrase ids start with the take's name (`A12`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Take {
    pub id: String,
    /// File name inside the footage folder.
    pub file: String,
    pub path: PathBuf,
    pub media: MediaInfo,
}

impl Take {
    pub fn stem(&self) -> &str {
        Path::new(&self.file).file_stem().and_then(OsStr::to_str).unwrap_or(&self.file)
    }

    /// A still image.
    pub fn still(&self) -> bool {
        self.media.video.as_ref().is_some_and(|video| video.still)
    }

    pub fn has_sound(&self) -> bool {
        !self.media.audio.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct Project {
    pub footage_dir: PathBuf,
    pub takes: Vec<Take>,
}

impl Project {
    /// Lists and probes the takes in `footage_dir` (hidden files and folders skipped):
    /// footage and still images.
    pub fn open(footage_dir: &Path) -> Result<Self> {
        let footage_dir = std::path::absolute(footage_dir)?;
        let mut files = Vec::new();
        for entry in fs::read_dir(&footage_dir)? {
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else { continue };
            if media_kind(&name).is_some()
                && !name.starts_with('.')
                && entry.path().metadata().is_ok_and(|meta| meta.is_file())
            {
                files.push(name);
            }
        }
        files.sort();
        if !files.iter().any(|file| media_kind(file) == Some(MediaKind::Footage)) {
            return Err(EditError::NoFootage(footage_dir));
        }
        let ids_path = footage_dir.join(EDIT_DIR).join(TAKE_IDS);
        let known: BTreeMap<String, String> = match fs::read(&ids_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => rendered_ids(&footage_dir, &files),
        };
        let ids = assign_ids(&known, &files);
        if ids != known {
            fs::create_dir_all(footage_dir.join(EDIT_DIR))?;
            crate::transcribe::write_atomic(&ids_path, &serde_json::to_vec_pretty(&ids)?)?;
        }
        let mut takes = ids
            .iter()
            .filter(|(_, file)| files.contains(file))
            .map(|(id, file)| {
                let path = footage_dir.join(file);
                let media = match media_kind(file) {
                    Some(MediaKind::Image) => probe_image(&path)?,
                    _ => probe(&path)?,
                };
                Ok(Take { id: id.clone(), media, path, file: file.clone() })
            })
            .collect::<Result<Vec<_>>>()?;
        takes.sort_by(|a, b| a.id.len().cmp(&b.id.len()).then(a.id.cmp(&b.id)));
        Ok(Self { footage_dir, takes })
    }

    pub fn edit_dir(&self) -> PathBuf {
        self.footage_dir.join(EDIT_DIR)
    }

    /// `edit/<name>`, creating `edit/` when needed.
    pub fn edit_path(&self, name: &str) -> Result<PathBuf> {
        let path = self.edit_dir().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(path)
    }

    pub fn take(&self, id: &str) -> Result<&Take> {
        let id = id.trim();
        self.takes.iter().find(|take| take.id.eq_ignore_ascii_case(id) || take.file == id).ok_or_else(|| {
            let known: Vec<String> = self.takes.iter().map(|take| format!("{} ({})", take.id, take.file)).collect();
            EditError::invalid(format!("no take “{id}”; takes: {}", known.join(", ")))
        })
    }
}

/// The ids an edit rendered before take ids were kept (`edit/edl.json` lists each
/// take's file), so files added since do not rename the takes its plan refers to.
fn rendered_ids(footage_dir: &Path, files: &[String]) -> BTreeMap<String, String> {
    let Ok(bytes) = fs::read(footage_dir.join(EDIT_DIR).join("edl.json")) else { return BTreeMap::new() };
    let Ok(edl) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return BTreeMap::new() };
    let Some(sources) = edl["sources"].as_object() else { return BTreeMap::new() };
    sources
        .iter()
        .filter(|(id, _)| !id.is_empty() && id.chars().all(|c| c.is_ascii_uppercase()))
        .filter_map(|(id, path)| {
            let file = Path::new(path.as_str()?).file_name()?.to_str()?.to_owned();
            files.contains(&file).then(|| (id.clone(), file))
        })
        .collect()
}

/// Take ids by id: the ones files already have (`known`, id to file), and the next
/// free ones for new files in file-name order. An id whose file is gone is not given to
/// another file, so an old plan never points at the wrong footage.
fn assign_ids(known: &BTreeMap<String, String>, files: &[String]) -> BTreeMap<String, String> {
    let mut ids = known.clone();
    let named: HashSet<&String> = known.values().collect();
    let mut next = 0;
    for file in files.iter().filter(|file| !named.contains(file)) {
        while ids.contains_key(&take_id(next)) {
            next += 1;
        }
        ids.insert(take_id(next), file.clone());
    }
    ids
}

/// `A`..`Z`, then `AA`, `AB`, ...
pub fn take_id(index: usize) -> String {
    let mut n = index;
    let mut id = Vec::new();
    loop {
        id.push(b'A' + (n % 26) as u8);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    id.reverse();
    String::from_utf8(id).expect("ascii")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_ids_count_like_spreadsheet_columns() {
        assert_eq!(take_id(0), "A");
        assert_eq!(take_id(25), "Z");
        assert_eq!(take_id(26), "AA");
        assert_eq!(take_id(27), "AB");
        assert_eq!(take_id(26 * 27), "AAA");
    }

    #[test]
    fn a_new_file_gets_the_next_id_and_the_others_keep_theirs() {
        let files = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>();
        let first = assign_ids(&BTreeMap::new(), &files(&["b.mp4", "c.mp4"]));
        assert_eq!(first.get("A").map(String::as_str), Some("b.mp4"));
        assert_eq!(first.get("B").map(String::as_str), Some("c.mp4"));
        // a.mp4 sorts first but comes later: it does not take A.
        let second = assign_ids(&first, &files(&["a.mp4", "b.mp4", "c.mp4"]));
        assert_eq!(second.get("A").map(String::as_str), Some("b.mp4"));
        assert_eq!(second.get("C").map(String::as_str), Some("a.mp4"));
        // B's file is gone: its id stays taken.
        let third = assign_ids(&second, &files(&["a.mp4", "b.mp4", "d.png"]));
        assert_eq!(third.get("D").map(String::as_str), Some("d.png"));
        assert_eq!(assign_ids(&third, &files(&["a.mp4", "b.mp4", "d.png"])), third);
    }

    #[test]
    fn images_are_takes_but_not_footage() {
        assert_eq!(media_kind("talk.MP4"), Some(MediaKind::Footage));
        assert_eq!(media_kind("chart.png"), Some(MediaKind::Image));
        assert_eq!(media_kind("notes.txt"), None);
    }
}
