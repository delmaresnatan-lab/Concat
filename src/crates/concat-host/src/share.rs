// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors

//! Shared projects on disk: the `.concatshare` file.
//!
//! A share is a zip a project travels in:
//!
//! - `project.json` - the ordinary project document, its media and font
//!   paths rewritten to the files inside the package (`media/...`,
//!   `fonts/...`).
//! - `manifest.json` - the package's contract: which format of package it
//!   is, who packed it, and each file's id, name and SHA-256.
//! - `media/`, `fonts/` - the files themselves, copied in at pack time.
//!
//! Packing takes the open document through the engine's reader and writer,
//! the way [`crate::templates`] does; unpacking validates every digest
//! before a folder is made, then creates the project with
//! [`crate::projects::create`] - which refuses a folder that already holds
//! one - copies the files inside it and opens through the normal flow, so
//! no lock is touched until the project is a real one.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use concat_effects::Catalogue;
use concat_project::{DocumentSettings, Editor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;

use crate::projects;

const MANIFEST: &str = "manifest.json";
const DOCUMENT: &str = "project.json";
const MEDIA: &str = "media";
const FONTS: &str = "fonts";

/// The file extension a share carries. Every dialog and picker names it.
pub const EXTENSION: &str = "concatshare";

/// The version of the package format this build writes. A reader refuses
/// only what is newer than itself; older packages read like today's.
pub const FORMAT: u32 = 1;

/// The file extension with its dot, for dialogs that want one.
pub const EXTENSION_DOT: &str = ".concatshare";

/// What to do, at pack time, with media and fonts whose file has left the
/// disk. The default refuses and names them - a share that arrives half
/// there is nobody's idea of one - and a caller may ask that the gaps go
/// across as placeholders the receiver relinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    /// Refuse the share, naming each file that is gone. The fix is a
    /// relink, then share again.
    Refuse,
    /// Share anyway: each becomes a placeholder, the way a template's slot
    /// ships, and the receiver's relink resolves or removes it.
    Placeholder,
}

/// One file the manifest names, by the id its document knows it by.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct SharedFile {
    /// The media item's id in the document, or `font0`, `font1`, ... for
    /// the fonts in their document order.
    id: String,
    /// The file inside the package. `None` when the entry is shared as a
    /// placeholder, with nothing behind it to hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    /// The SHA-256 of the file, hex. Absent with `file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    /// The file's size in bytes. Informational; the digest is the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
}

/// The package's contract, as `manifest.json` holds it.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    /// [`FORMAT`], as the packing build wrote it.
    format: u32,
    /// The version of the app that packed it. Informational.
    concat: String,
    /// The project's name, and its folder's where it lands.
    name: String,
    video: Video,
    /// Every media item of the document, in its order.
    media: Vec<SharedFile>,
    /// Every custom font of the document, in its order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fonts: Vec<SharedFile>,
}

/// The frame the project renders at, as the manifest restates it.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Video {
    width: u32,
    height: u32,
    rate_num: i64,
    rate_den: i64,
    /// `#rrggbb` or `#rrggbbaa`, left out while opaque black; see
    /// `VideoSettings::canvas`.
    #[serde(
        default = "opaque_black",
        skip_serializing_if = "is_black",
        with = "concat_project::model::hex_rgba"
    )]
    canvas: [u8; 4],
}

fn opaque_black() -> [u8; 4] {
    concat_project::model::OPAQUE_BLACK
}

fn is_black(rgba: &[u8; 4]) -> bool {
    *rgba == concat_project::model::OPAQUE_BLACK
}

/// One file on its way into the package: the document id that owns it, the
/// entry it will travel under, and where it is on this machine now.
struct Packed {
    id: String,
    entry: String,
    source: PathBuf,
}

/// Packs the given document into a new `.concatshare` file at `out_file`.
///
/// The document is the open one, its media paths absolute on this machine;
/// `settings` is the session's. Media and fonts are copied into the
/// package, each named after its id, and the document's paths rewritten to
/// the package-relative names. A file that has left the disk is refused
/// and named, unless `missing` asks that it go across as a placeholder.
pub fn pack(
    document: &Value,
    settings: &DocumentSettings,
    out_file: &str,
    missing: Missing,
) -> Result<(), String> {
    // Typed through the engine's reader and writer, not hand-edited JSON:
    // the packed document is written by the same code path every save
    // uses, and a schema change breaks this at compile time.
    let Some(mut project) = concat_project::from_document(document) else {
        return Err("the open project is not a document this build can read".to_owned());
    };

    // A placeholder ships with no file - it was the creator's stand-in.
    // Everything else needs its file on the disk to go in the package.
    let gone: Vec<(String, String)> = project
        .media
        .iter()
        .filter(|item| !item.placeholder && !Path::new(&item.path).is_file())
        .map(|item| (item.name.clone(), item.path.clone()))
        .chain(
            project
                .fonts
                .iter()
                .filter(|font| !Path::new(&font.path).is_file())
                .map(|font| (font.family.clone(), font.path.clone())),
        )
        .collect();
    if !gone.is_empty() {
        let list = gone
            .iter()
            .map(|(name, path)| format!("{name:?} ({path})"))
            .collect::<Vec<_>>()
            .join(", ");
        match missing {
            Missing::Refuse => {
                return Err(format!(
                    "these files are not on the disk to pack: {list}; relink them and share again, or share once more choosing to leave them out"
                ));
            }
            // A gap becomes a placeholder, the way a template's slot
            // ships; the receiver's relink resolves or removes it.
            Missing::Placeholder => {
                for item in &mut project.media {
                    if !item.placeholder && !Path::new(&item.path).is_file() {
                        item.path = String::new();
                        item.placeholder = true;
                    }
                }
                for font in &mut project.fonts {
                    if !Path::new(&font.path).is_file() {
                        font.path = String::new();
                    }
                }
            }
        }
    }

    let mut packed: Vec<Packed> = Vec::new();
    for item in &mut project.media {
        if item.placeholder || item.path.is_empty() {
            continue;
        }
        let source = item.path.clone();
        let entry = shared_file(MEDIA, &item.id, &source);
        item.path = entry.clone();
        packed.push(Packed {
            id: item.id.clone(),
            entry,
            source: PathBuf::from(source),
        });
    }
    for (index, font) in project.fonts.iter_mut().enumerate() {
        if font.path.is_empty() {
            continue;
        }
        let source = font.path.clone();
        let entry = shared_file(FONTS, &format!("font{index}"), &source);
        font.path = entry.clone();
        packed.push(Packed {
            id: format!("font{index}"),
            entry,
            source: PathBuf::from(source),
        });
    }

    // The names are the ids and basenames this project already carries, so
    // the entries are unique without depending on what the file was called.
    let document = concat_project::to_document(settings, &project);

    let file = std::fs::File::create(out_file)
        .map_err(|error| format!("could not create {out_file}: {error}"))?;
    let mut zip = zip::ZipWriter::new(file);
    let outcome = write_package(&mut zip, &document, settings, &packed);
    if outcome.is_err() {
        // A half-written package is worse than none; the save dialog that
        // chose the name has already had its say about overwriting.
        drop(zip);
        let _ = std::fs::remove_file(out_file);
    }
    outcome
}

/// Writes the document, every file, and the manifest that names their
/// digests, into `zip`.
fn write_package(
    zip: &mut zip::ZipWriter<std::fs::File>,
    document: &Value,
    settings: &DocumentSettings,
    packed: &[Packed],
) -> Result<(), String> {
    let options = || {
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
    };
    let encoded = serde_json::to_vec_pretty(document)
        .map_err(|error| format!("could not encode the project: {error}"))?;
    zip.start_file(DOCUMENT, options())
        .map_err(|error| format!("could not write the package: {error}"))?;
    zip.write_all(&encoded)
        .map_err(|error| format!("could not write the package: {error}"))?;

    let mut media: Vec<SharedFile> = Vec::new();
    let mut fonts: Vec<SharedFile> = Vec::new();
    for file in packed {
        let (sha256, bytes) = pack_entry(zip, &file.entry, &file.source)?;
        let shared = SharedFile {
            id: file.id.clone(),
            file: Some(file.entry.clone()),
            sha256: Some(sha256),
            bytes: Some(bytes),
        };
        if file.entry.starts_with(FONTS) {
            fonts.push(shared);
        } else {
            media.push(shared);
        }
    }

    // A placeholder ships named but empty, so the receiver's unpack hands
    // it back as the gap it is instead of reading it as missing media.
    for item in document.get(MEDIA).and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !media.iter().any(|shared| shared.id == id) {
            media.push(SharedFile {
                id: id.to_owned(),
                file: None,
                sha256: None,
                bytes: None,
            });
        }
    }

    let manifest = Manifest {
        format: FORMAT,
        concat: env!("CARGO_PKG_VERSION").to_owned(),
        name: settings.name.clone(),
        video: Video {
            width: settings.width,
            height: settings.height,
            rate_num: settings.rate_num,
            rate_den: settings.rate_den,
            canvas: settings.canvas,
        },
        media,
        fonts,
    };
    let encoded = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("could not encode the manifest: {error}"))?;
    zip.start_file(MANIFEST, options())
        .map_err(|error| format!("could not write the package: {error}"))?;
    zip.write_all(&encoded)
        .map_err(|error| format!("could not write the package: {error}"))
}

/// Copies one source file into the package at `entry`, digesting the bytes
/// as they go, so the manifest's hash is of what actually went in.
fn pack_entry(
    zip: &mut zip::ZipWriter<std::fs::File>,
    entry: &str,
    source: &Path,
) -> Result<(String, u64), String> {
    let mut file = std::fs::File::open(source)
        .map_err(|error| format!("could not read {}: {error}", source.display()))?;
    zip.start_file(entry, {
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)
    })
    .map_err(|error| format!("could not write {entry} into the package: {error}"))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut bytes = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("could not read {}: {error}", source.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        zip.write_all(&buffer[..read])
            .map_err(|error| format!("could not write {entry} into the package: {error}"))?;
        bytes += read as u64;
    }
    Ok((hex(&hasher.finalize()), bytes))
}

/// The entry name a file of `section` travels under: its id and the name
/// it came with, both made filesystem-safe - the same scheme the template
/// bundles use, which gives unique, stable names.
fn shared_file(section: &str, id: &str, source: &str) -> String {
    let base = Path::new(source)
        .file_name()
        .and_then(|name| name.to_str())
        .map(projects::folder_name)
        .unwrap_or_else(|| "file".to_owned());
    format!("{section}/{}-{base}", projects::folder_name(id))
}

/// Opens a `.concatshare` file as a new project under `location`: the
/// manifest and every digest are checked before a folder is made, the
/// package's media and fonts move inside the project, and the document is
/// repointed at them. Returns the project, ready to open.
pub fn unpack(file: &str, location: &str) -> Result<projects::ProjectInfo, String> {
    let handle = std::fs::File::open(file)
        .map_err(|error| format!("could not open {file}: {error}"))?;
    let mut archive = zip::ZipArchive::new(handle)
        .map_err(|error| format!("{file} is not a shared Concat project: {error}"))?;

    let manifest: Manifest = {
        let bytes = read_entry(&mut archive, MANIFEST, file)?;
        serde_json::from_slice(&bytes).map_err(|error| {
            format!("the manifest of {file} is not one this build can read: {error}")
        })?
    };
    if manifest.format > FORMAT {
        return Err(format!(
            "the package is format {}, and this build reads up to {FORMAT}; update Concat to open it",
            manifest.format
        ));
    }

    // The document, through the engine's own reader, before anything is
    // made: a package this build cannot open refuses whole.
    let mut document: Value = {
        let bytes = read_entry(&mut archive, DOCUMENT, file)?;
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("the project in {file} does not read: {error}"))?
    };
    if concat_project::from_document(&document).is_none() {
        return Err("the package holds a project this build cannot read".to_owned());
    }

    // Every media id and font the document carries must be one the
    // manifest names; one left out would open as missing media with its
    // file not even in the package.
    let covered = |section: &str, entries: &[SharedFile]| -> Result<(), String> {
        let Some(items) = document.get(section).and_then(Value::as_array) else {
            return Ok(());
        };
        for item in items {
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            if !entries.iter().any(|shared| shared.id == id) {
                return Err(format!("the manifest does not name {section} {id:?}"));
            }
        }
        Ok(())
    };
    covered(MEDIA, &manifest.media)?;
    covered(FONTS, &manifest.fonts)?;

    // Every digest, checked before a folder is made: a package that fails
    // here leaves nothing behind.
    for shared in manifest.media.iter().chain(manifest.fonts.iter()) {
        let Some(entry) = &shared.file else {
            continue;
        };
        if !safe_entry(entry) {
            return Err(format!(
                "the manifest names {entry:?}, which is not a file of this package"
            ));
        }
        let (digest, bytes) = hash_entry(&mut archive, entry)?;
        if shared.sha256.as_deref() != Some(digest.as_str())
            || shared.bytes.is_some_and(|expected| expected != bytes)
        {
            return Err(format!("{entry} arrived damaged"));
        }
    }

    let project = projects::create(
        location,
        &manifest.name,
        manifest.video.width,
        manifest.video.height,
        manifest.video.rate_num,
        manifest.video.rate_den,
        manifest.video.canvas,
    )?;
    let root = PathBuf::from(&project.path);

    // The package's files move inside the project, so it stands alone on
    // the machine it lands on, and the document is repointed at them.
    for (section, entries) in [(MEDIA, &manifest.media), (FONTS, &manifest.fonts)] {
        let Some(items) = document.get_mut(section).and_then(Value::as_array_mut) else {
            continue;
        };
        for item in items {
            // Media match on the id the document and the manifest both
            // carry; fonts have no id - pack rewrote their path to the
            // package entry, which is what names them here.
            let shared = if section == MEDIA {
                let id = item.get("id").and_then(Value::as_str);
                id.and_then(|id| entries.iter().find(|shared| shared.id == id))
            } else {
                let path = item.get("path").and_then(Value::as_str);
                path.and_then(|path| {
                    entries
                        .iter()
                        .find(|shared| shared.file.as_deref() == Some(path))
                })
            };
            let Some(shared) = shared else {
                continue;
            };
            match &shared.file {
                Some(entry) => {
                    let destination = root.join(entry);
                    if let Some(parent) = destination.parent()
                        && let Err(error) = std::fs::create_dir_all(parent)
                    {
                        return Err(format!(
                            "could not create {}: {error}",
                            parent.display()
                        ));
                    }
                    copy_entry(&mut archive, entry, &destination)?;
                    item["path"] = Value::String(destination.to_string_lossy().into_owned());
                }
                // Nothing behind it: a placeholder, for the receiver's
                // relink to resolve or remove.
                None => {
                    item["path"] = Value::String(String::new());
                    if section == MEDIA {
                        item["placeholder"] = Value::Bool(true);
                    }
                }
            }
        }
    }

    // Read and written the way every open is: links that this build has
    // since retired read as links to their stand-ins.
    let mut editor = Editor::from_document(&document)
        .ok_or("the package holds a project this build cannot read".to_owned())?;
    let catalogue = Catalogue::builtin();
    editor.upgrade_links(|link| catalogue.upgrade(link));
    let settings = DocumentSettings {
        name: project.name.clone(),
        width: manifest.video.width,
        height: manifest.video.height,
        rate_num: manifest.video.rate_num,
        rate_den: manifest.video.rate_den,
        canvas: editor.project().active().video.canvas,
    };
    projects::save(&project.path, &editor.to_document(&settings))?;

    Ok(project)
}

/// Reads one entry whole out of the package.
fn read_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    file: &str,
) -> Result<Vec<u8>, String> {
    let mut entry = archive
        .by_name(name)
        .map_err(|_| format!("{file} is not a shared Concat project: {name} is missing"))?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read {name} out of {file}: {error}"))?;
    Ok(bytes)
}

/// Digests one entry of the package as it streams past.
fn hash_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    entry: &str,
) -> Result<(String, u64), String> {
    let mut file = archive
        .by_name(entry)
        .map_err(|_| format!("the package does not carry {entry}"))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut bytes = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("could not read {entry} out of the package: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes += read as u64;
    }
    Ok((hex(&hasher.finalize()), bytes))
}

/// Copies one entry of the package to a file on the disk.
fn copy_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    entry: &str,
    destination: &Path,
) -> Result<(), String> {
    let mut source = archive
        .by_name(entry)
        .map_err(|_| format!("the package does not carry {entry}"))?;
    let mut file = std::fs::File::create(destination)
        .map_err(|error| format!("could not write {}: {error}", destination.display()))?;
    std::io::copy(&mut source, &mut file)
        .map_err(|error| format!("could not copy {entry} into the project: {error}"))?;
    Ok(())
}

/// Whether `name` is an entry the manifest may point at: inside `media/`
/// or `fonts/`, with no way out of it. Everything else is a malformed
/// package, refused before any folder is made.
fn safe_entry(name: &str) -> bool {
    (name.starts_with(&format!("{MEDIA}/")) || name.starts_with(&format!("{FONTS}/")))
        && !Path::new(name).components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings() -> DocumentSettings {
        DocumentSettings {
            name: "Beach day".to_owned(),
            width: 1920,
            height: 1080,
            rate_num: 30,
            rate_den: 1,
            canvas: [0, 0, 0, 255],
        }
    }

    /// A project of one placeholder and one real clip, as a save writes it.
    fn document(clip: &Path) -> Value {
        json!({
            "concat": "0.2.12", "version": 1, "name": "Beach day",
            "video": { "width": 1920, "height": 1080, "rateNum": 30, "rateDen": 1 },
            "media": [
                { "id": "m1", "path": "", "name": "Title card", "duration": 4.0,
                  "kind": "video", "hasAudio": false, "placeholder": true },
                { "id": "m2", "path": clip.to_string_lossy(), "name": "clip.mp4",
                  "duration": 2.0, "kind": "video", "hasAudio": true }
            ],
            "tracks": [{ "id": "T1", "name": "Track 1", "visible": true, "muted": false }],
            "clips": [
                { "id": "c1", "trackId": "T1", "mediaId": "m1", "name": "Title card",
                  "kind": "video", "start": 0.0, "duration": 4.0, "sourceStart": 0.0,
                  "volume": 1.0, "fadeIn": 0.0, "fadeOut": 0.0, "scale": 1.0,
                  "offsetX": 0.0, "offsetY": 0.0, "rotation": 0.0, "opacity": 1.0,
                  "speed": 1.0, "preservePitch": true, "filters": [], "videoEffects": [] },
                { "id": "c2", "trackId": "T1", "mediaId": "m2", "name": "clip.mp4",
                  "kind": "video", "start": 4.0, "duration": 2.0, "sourceStart": 0.0,
                  "volume": 1.0, "fadeIn": 0.0, "fadeOut": 0.0, "scale": 1.0,
                  "offsetX": 0.0, "offsetY": 0.0, "rotation": 0.0, "opacity": 1.0,
                  "speed": 1.0, "preservePitch": true, "filters": [], "videoEffects": [] }
            ]
        })
    }

    /// A scratch folder swept if it is already there.
    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "concat-share-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch dir");
        root
    }

    #[test]
    fn packing_a_project_and_it_round_trips() {
        let scratch = scratch("round-trip");
        let media = scratch.join("clips");
        std::fs::create_dir_all(&media).expect("scratch dir");
        let clip = media.join("clip.mp4");
        std::fs::write(&clip, b"not really an mp4").expect("writes");

        let out = scratch.join("beach.concatshare");
        pack(&document(&clip), &settings(), &out.to_string_lossy(), Missing::Refuse)
            .expect("packs");

        // The package holds the document, the manifest and the file.
        let handle = std::fs::File::open(&out).expect("opens");
        let mut archive = zip::ZipArchive::new(handle).expect("a zip");
        assert!(archive.by_name(DOCUMENT).is_ok(), "the document is in it");
        let manifest: Manifest = {
            let bytes = read_entry(&mut archive, MANIFEST, "beach").expect("manifest");
            serde_json::from_slice(&bytes).expect("parses")
        };
        assert_eq!(manifest.format, FORMAT);
        assert_eq!(manifest.media.len(), 2, "the placeholder is named too");
        assert!(
            manifest
                .media
                .iter()
                .find(|shared| shared.id == "m1")
                .is_some_and(|shared| shared.file.is_none()),
            "but has no file"
        );

        let location = scratch.join("projects");
        std::fs::create_dir_all(&location).expect("scratch dir");
        let project = unpack(&out.to_string_lossy(), &location.to_string_lossy())
            .expect("unpacks");

        let opened = projects::read_document(&project.path)
            .expect("reads back")
            .expect("a document");
        let media = opened["media"].as_array().expect("media");
        assert_eq!(media[0]["path"], "", "the placeholder stays a placeholder");
        let repointed = media[1]["path"].as_str().expect("path");
        assert!(
            Path::new(repointed).is_absolute() && Path::new(repointed).is_file(),
            "the clip was copied into the project and repointed: {repointed}"
        );
        assert!(
            Path::new(&project.path).join("media").is_dir(),
            "the clip lives inside the project folder"
        );
        let clips = opened["clips"].as_array().expect("clips");
        assert_eq!(clips[1]["mediaId"], "m2", "the edit travels untouched");

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn packing_refuses_media_that_has_left_the_disk_and_names_it() {
        let scratch = scratch("refuse");
        let gone = scratch.join("gone.mp4");
        let out = scratch.join("beach.concatshare");
        let error = pack(
            &document(&gone),
            &settings(),
            &out.to_string_lossy(),
            Missing::Refuse,
        )
        .expect_err("refuses");
        assert!(error.contains("gone.mp4"), "{error}");
        assert!(!out.exists(), "nothing was written");

        // Asked to share anyway, the gap goes across as a placeholder.
        pack(
            &document(&gone),
            &settings(),
            &out.to_string_lossy(),
            Missing::Placeholder,
        )
        .expect("packs");
        let location = scratch.join("projects");
        std::fs::create_dir_all(&location).expect("scratch dir");
        let project = unpack(&out.to_string_lossy(), &location.to_string_lossy())
            .expect("unpacks");
        let opened = projects::read_document(&project.path)
            .expect("reads back")
            .expect("a document");
        let media = opened["media"].as_array().expect("media");
        assert_eq!(media[1]["path"], "", "the missing clip is a placeholder");
        assert_eq!(
            media[1]["placeholder"], true,
            "and the engine reads it as one"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// Copies a package of one's own with the manifest replaced - the way
    /// a package from anywhere else might arrive.
    fn re_manifested(name: &str, good: &Path, manifest: Value) -> PathBuf {
        let out = scratch(name).join("tampered.concatshare");
        let mut source = zip::ZipArchive::new(std::fs::File::open(good).expect("opens"))
            .expect("a zip");
        let file = std::fs::File::create(&out).expect("creates");
        let mut zip = zip::ZipWriter::new(file);
        let options =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let names: Vec<String> = (0..source.len())
            .filter_map(|index| source.by_index(index).ok().map(|f| f.name().to_owned()))
            .collect();
        for name in names {
            if name == MANIFEST {
                continue;
            }
            let mut from = source.by_name(&name).expect("entry");
            let mut bytes = Vec::new();
            from.read_to_end(&mut bytes).expect("reads");
            zip.start_file(&name, options.clone()).expect("starts");
            zip.write_all(&bytes).expect("writes");
        }
        zip.start_file(MANIFEST, options).expect("starts");
        zip.write_all(&serde_json::to_vec(&manifest).expect("encodes"))
            .expect("writes");
        zip.finish().expect("finishes");
        out
    }

    /// Writes a zip with nothing but a manifest - enough for the checks
    /// that run before the document is read.
    fn manifest_only(name: &str, manifest: Value) -> PathBuf {
        let out = scratch(name).join("tampered.concatshare");
        let file = std::fs::File::create(&out).expect("creates");
        let mut zip = zip::ZipWriter::new(file);
        let options =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(MANIFEST, options).expect("starts");
        zip.write_all(&serde_json::to_vec(&manifest).expect("encodes"))
            .expect("writes");
        zip.finish().expect("finishes");
        out
    }

    #[test]
    fn a_package_from_a_newer_concat_is_refused() {
        let manifest = json!({ "format": FORMAT + 1, "concat": "999", "name": "x",
            "video": { "width": 1920, "height": 1080, "rateNum": 30, "rateDen": 1 },
            "media": [] });
        let out = manifest_only("newer-format", manifest);
        let error = unpack(&out.to_string_lossy(), &scratch("newer-projects").to_string_lossy())
            .expect_err("refuses");
        assert!(error.contains("format"), "{error}");
    }

    #[test]
    fn a_damaged_file_is_refused_before_any_folder_is_made() {
        let folder = scratch("damaged");
        let clip = folder.join("clip.mp4");
        std::fs::write(&clip, b"not really an mp4").expect("writes");
        let document = document(&clip);

        let good = folder.join("good.concatshare");
        pack(&document, &settings(), &good.to_string_lossy(), Missing::Refuse)
            .expect("packs");
        let manifest: Value = {
            let handle = std::fs::File::open(&good).expect("opens");
            let mut archive = zip::ZipArchive::new(handle).expect("a zip");
            let bytes = read_entry(&mut archive, MANIFEST, "good").expect("manifest");
            serde_json::from_slice(&bytes).expect("parses")
        };

        // A digest that names different bytes than the package carries.
        let mut lies = manifest.clone();
        lies["media"][0]["sha256"] = json!(hex(&[0u8; 32]));
        let out = re_manifested("damaged-lies", &good, lies);
        let error = unpack(&out.to_string_lossy(), &scratch("damaged-projects").to_string_lossy())
            .expect_err("refuses");
        assert!(error.contains("damaged"), "{error}");

        // An entry the manifest points at but the package does not carry.
        let mut absent = manifest;
        absent["media"][0]["file"] = json!("media/m99-nobody.mp4");
        let out = re_manifested("damaged-absent", &good, absent);
        let error = unpack(&out.to_string_lossy(), &scratch("damaged-projects").to_string_lossy())
            .expect_err("refuses");
        assert!(error.contains("m99-nobody"), "{error}");

        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn an_entry_outside_the_package_is_refused() {
        let folder = scratch("traversal");
        let clip = folder.join("clip.mp4");
        std::fs::write(&clip, b"not really an mp4").expect("writes");
        let document = document(&clip);

        let good = folder.join("good.concatshare");
        pack(&document, &settings(), &good.to_string_lossy(), Missing::Refuse)
            .expect("packs");
        let mut manifest: Value = {
            let handle = std::fs::File::open(&good).expect("opens");
            let mut archive = zip::ZipArchive::new(handle).expect("a zip");
            let bytes = read_entry(&mut archive, MANIFEST, "good").expect("manifest");
            serde_json::from_slice(&bytes).expect("parses")
        };

        for escape in ["../clip.mp4", "/etc/passwd", "media/../../clip.mp4"] {
            manifest["media"][0]["file"] = json!(escape);
            let out = re_manifested("traversal-attempt", &good, manifest.clone());
            let error = unpack(
                &out.to_string_lossy(),
                &scratch("traversal-projects").to_string_lossy(),
            )
            .expect_err("refuses");
            assert!(error.contains("not a file of this package"), "{error}");
        }

        let _ = std::fs::remove_dir_all(&folder);
    }
}
