//! Bringing material into the footage folder: a file from elsewhere on disk, or a
//! download. It becomes a take like the footage the creator added.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{EditError, Result};
use crate::project::{FOOTAGE_EXTENSIONS, IMAGE_EXTENSIONS, media_kind};
use crate::tool;

/// Where files are put together before they get their name in the footage folder:
/// hidden, so no half-written file is ever listed as a take.
const STAGING: &str = ".rushes-import";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(3600);

/// Copies `source` into `footage_dir` under `name` (default its own name; the extension
/// stays the source's), or the next free `name (2).ext`. Returns the file name.
pub fn copy_in(footage_dir: &Path, source: &Path, name: Option<&str>) -> Result<String> {
    let meta = fs::metadata(source).map_err(|error| EditError::invalid(format!("{}: {error}", source.display())))?;
    if !meta.is_file() {
        return Err(EditError::invalid(format!("{} is not a file", source.display())));
    }
    let file_name = source.file_name().and_then(|name| name.to_str()).unwrap_or_default().to_owned();
    check_kind(&file_name)?;
    if source.parent().and_then(|dir| fs::canonicalize(dir).ok()) == fs::canonicalize(footage_dir).ok() {
        return Ok(file_name);
    }
    let staging = staging_dir(footage_dir)?;
    let staged = staging.join(&file_name);
    fs::copy(source, &staged)?;
    place(footage_dir, &staged, &target_name(&file_name, name))
}

/// Downloads `url` into `footage_dir`: a direct link to a media file or image as it is,
/// any other page (YouTube and the like) with yt-dlp, at up to 1080p. Returns the file
/// name.
pub fn download(footage_dir: &Path, url: &str, name: Option<&str>, progress: &(dyn Fn(&str) + Sync)) -> Result<String> {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(EditError::invalid(format!("“{url}” is not an http(s) link")));
    }
    let staging = staging_dir(footage_dir)?.join(format!("download-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let result = (|| {
        let staged = match direct_file_name(url) {
            Some(file_name) => {
                progress(&format!("downloading {file_name}"));
                fetch(url, &staging.join(&file_name))?
            }
            None => {
                progress("downloading with yt-dlp");
                yt_dlp(url, &staging)?
            }
        };
        let file_name = staged.file_name().and_then(|name| name.to_str()).unwrap_or_default().to_owned();
        check_kind(&file_name)?;
        place(footage_dir, &staged, &target_name(&file_name, name))
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

fn staging_dir(footage_dir: &Path) -> Result<PathBuf> {
    let dir = footage_dir.join(STAGING);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn check_kind(file_name: &str) -> Result<()> {
    if media_kind(file_name).is_none() {
        return Err(EditError::invalid(format!(
            "{file_name} is neither footage ({}) nor an image ({}): convert it first",
            FOOTAGE_EXTENSIONS.join(", "),
            IMAGE_EXTENSIONS.join(", ")
        )));
    }
    Ok(())
}

/// `name` with the extension of `file_name`, or `file_name` itself; never a path.
fn target_name(file_name: &str, name: Option<&str>) -> String {
    let extension = Path::new(file_name).extension().and_then(|ext| ext.to_str()).unwrap_or_default();
    let name = name.map(str::trim).filter(|name| !name.is_empty());
    let stem = match name {
        Some(name) => Path::new(name).file_stem().and_then(|stem| stem.to_str()).unwrap_or(name).to_owned(),
        None => Path::new(file_name).file_stem().and_then(|stem| stem.to_str()).unwrap_or(file_name).to_owned(),
    };
    let stem: String = stem.chars().map(|c| if c == '/' || c == '\\' || c.is_control() { '_' } else { c }).collect();
    let stem = stem.trim_start_matches('.');
    format!("{}.{extension}", if stem.is_empty() { "insert" } else { stem })
}

/// Moves `staged` into `footage_dir` as `name`, or `name (2)`, `name (3)`, … when taken.
fn place(footage_dir: &Path, staged: &Path, name: &str) -> Result<String> {
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or(name);
    let extension = path.extension().and_then(|ext| ext.to_str()).map(|ext| format!(".{ext}")).unwrap_or_default();
    for n in 1.. {
        let candidate = if n == 1 { name.to_owned() } else { format!("{stem} ({n}){extension}") };
        let target = footage_dir.join(&candidate);
        if target.exists() {
            continue;
        }
        fs::rename(staged, &target)?;
        return Ok(candidate);
    }
    unreachable!("an unbounded range always yields a free name")
}

/// The file name of a direct link to footage or an image (`…/clip.mp4?x=1`).
fn direct_file_name(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let last = path.rsplit('/').next()?;
    let name = percent_decode(last);
    (media_kind(&name).is_some() && !name.starts_with('.')).then_some(name)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |at: usize| bytes.get(at).and_then(|b| (*b as char).to_digit(16));
        match (bytes[index], hex(index + 1), hex(index + 2)) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                index += 3;
            }
            (byte, _, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn fetch(url: &str, to: &Path) -> Result<PathBuf> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut response = agent.get(url).call().map_err(|error| EditError::invalid(format!("{url}: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(EditError::invalid(format!("{url} answered {status}")));
    }
    let mut file = fs::File::create(to)?;
    io::copy(&mut response.body_mut().as_reader(), &mut file)?;
    file.sync_all()?;
    Ok(to.to_owned())
}

fn yt_dlp(url: &str, dir: &Path) -> Result<PathBuf> {
    let program = tool::find("yt-dlp").ok_or_else(|| {
        EditError::invalid(
            "yt-dlp is not installed, so only direct links to a file can be downloaded: install yt-dlp, or \
             download it another way and add it by path",
        )
    })?;
    let template = dir.join("%(title).80B [%(id)s].%(ext)s");
    let stdout = tool::run(
        &program,
        [
            "--no-playlist".as_ref(),
            "--no-progress".as_ref(),
            "--quiet".as_ref(),
            "-f".as_ref(),
            "bv*[height<=1080]+ba/b[height<=1080]/bv*+ba/b".as_ref(),
            "--merge-output-format".as_ref(),
            "mp4".as_ref(),
            "-o".as_ref(),
            template.as_os_str(),
            "--print".as_ref(),
            "after_move:filepath".as_ref(),
            url.as_ref(),
        ],
    )?;
    let printed = String::from_utf8_lossy(&stdout);
    let path = printed.lines().map(str::trim).rfind(|line| !line.is_empty()).map(PathBuf::from);
    path.filter(|path| path.is_file())
        .ok_or_else(|| EditError::invalid(format!("yt-dlp downloaded nothing from {url}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_links_name_their_file() {
        assert_eq!(direct_file_name("https://x.org/a/clip%201.mp4?sig=2").as_deref(), Some("clip 1.mp4"));
        assert_eq!(direct_file_name("https://x.org/shot.PNG#top").as_deref(), Some("shot.PNG"));
        assert_eq!(direct_file_name("https://www.youtube.com/watch?v=e7ZwqHjQh5U"), None);
    }

    #[test]
    fn names_keep_the_source_extension_and_stay_in_the_folder() {
        assert_eq!(target_name("prev.mp4", None), "prev.mp4");
        assert_eq!(target_name("prev.mp4", Some("old video")), "old video.mp4");
        assert_eq!(target_name("prev.mp4", Some("../up/x.mov")), "x.mp4");
        assert_eq!(target_name("a.png", Some(".hidden")), "hidden.png");
    }

    #[test]
    fn a_file_is_copied_in_under_a_free_name() {
        let root = tempfile::tempdir().unwrap();
        let footage = root.path().join("footage");
        fs::create_dir_all(&footage).unwrap();
        fs::write(footage.join("chart.png"), b"old").unwrap();
        let source = root.path().join("chart.png");
        fs::write(&source, b"new").unwrap();
        assert_eq!(copy_in(&footage, &source, None).unwrap(), "chart (2).png");
        assert_eq!(fs::read(footage.join("chart (2).png")).unwrap(), b"new");
        assert!(source.is_file(), "the source stays");
        // Already in the folder: nothing to copy.
        assert_eq!(copy_in(&footage, &footage.join("chart.png"), None).unwrap(), "chart.png");
        assert!(copy_in(&footage, &root.path().join("missing.mp4"), None).is_err());
        fs::write(root.path().join("notes.txt"), b"x").unwrap();
        assert!(copy_in(&footage, &root.path().join("notes.txt"), None).is_err());
    }
}
