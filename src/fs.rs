use std::{collections::{HashMap, HashSet}, io::Write, path::PathBuf, sync::atomic::AtomicBool};

use camino::Utf8PathBuf;
use serde::Serialize;
use smash_arc::{Hash40, ArcLookup, hash40};
use thiserror::Error;

use self::interner::InternedPath;
use crate::{hashes, FILESYSTEM};

// pub mod api;
// mod event;

mod discover;
pub mod interner;
pub use discover::*;

static DEFAULT_CONFIG: &str = include_str!("../resources/override.json");

#[derive(Default)]
pub struct ModFileSystem {
    files: HashMap<Hash40, Utf8PathBuf>,
    incoming_file: Option<Hash40>,
    remaining_bytes: usize,
}

impl ModFileSystem {
    pub fn new(files: HashMap<Hash40, Utf8PathBuf>) -> Self {
        Self { files, incoming_file: None, remaining_bytes: 0 }
    }

    // NOTE: Some sources such as API callbacks cannot provide a physical path. This needs proper handling
    pub fn get_physical_path<H: Into<Hash40>>(&self, hash: H) -> Option<Utf8PathBuf> {
        self.files.get(&hash.into()).map(|path| path.to_owned())
    }

    pub fn set_incoming_file<H: Into<Hash40>>(&mut self, hash: H) {
        if let Some(hash) = self.incoming_file.take() {
            println!(
            "Removing file '{}' ({:#x}) from incoming load before using it.",
                hashes::find(hash),
                hash.0
            );
        }

        let hash = hash.into();
        
        self.incoming_file = Some(hash);
        self.remaining_bytes = std::fs::metadata(self.files.get(&hash).unwrap()).unwrap().len() as _;
    }

    pub fn get_incoming_file(&mut self) -> Option<Hash40> {
        self.incoming_file.take()
    }

    pub fn sub_remaining_bytes(&mut self, size: usize) -> Option<Hash40> {
        if size >= self.remaining_bytes {
            self.get_incoming_file()
        } else {
            self.remaining_bytes -= size;
            None
        }
    }

    pub fn load_file_into<H: Into<Hash40>, B: AsMut<[u8]>>(&self, hash: H, mut buffer: B) -> Result<usize, ModpackError> {
        let hash = hash.into();
        let data = std::fs::read(FILESYSTEM.get().unwrap().get(&hash).unwrap())?;
        // let data = self.load(hash)?;
        buffer.as_mut().write_all(&data)?;
        Ok(data.len())
    }

    pub fn load<H: Into<Hash40>>(&self, hash: H) -> Result<Vec<u8>, ModpackError> {
        let hash = hash.into();
        let path = self.get_physical_path(hash).unwrap();
        println!("Path: {}", path);
        Ok(std::fs::read(path).unwrap())
    }
}

/// The user's set of mods presented in a way that makes referencing easy.
/// Ultimately this should only be used for files physically present, so no API stuff.
#[derive(Default)]
pub struct Modpack {
    pub mods: Vec<ModDir>,
    // files: HashMap<Hash40, InternedPath<{ discover::MAX_COMPONENT_COUNT }>>,
}

pub fn look_for_conflicts(_modpack: Modpack) {

}

pub fn get_additional_files(files: &mut Vec<ModFile>) -> Vec<ModFile> {
    let arc = crate::resource::arc();
    files.drain_filter(|file| arc.get_file_path_index_from_hash(hash40(file.path.as_str())).is_ok() ).collect()
}

#[repr(transparent)]
pub struct UnconflictingModpack(Modpack);

#[derive(Error, Debug)]
pub enum ModpackError {
    #[error("could not write file to the buffer")]
    IoError(#[from] std::io::Error),
    #[error("failed to find the file {} in the filesystem", hashes::find(*.0))]
    FileMissing(Hash40),
}

impl Modpack {
    
}

#[derive(Default, Clone, PartialEq, Eq)]
pub struct ModDir {
    pub root: Utf8PathBuf,
    pub files: Vec<ModFile>,
    patches: Vec<Utf8PathBuf>,
}

impl ModDir {
    pub fn get_patch(&self) -> Vec<(Hash40, u64)> {
        self.files.iter().map(|file| (hash40(file.path.strip_prefix(&self.root).unwrap().as_str()), file.size)).collect()
    }

    pub fn get_filesystem(&self) -> HashMap<Hash40, Utf8PathBuf> {
        self.files.iter().map(|file| (hash40(file.path.strip_prefix(&self.root).unwrap().as_str()), file.path.to_owned())).collect()
    }
}

pub fn check_for_conflicts(mut modpack:  Modpack) -> (UnconflictingModpack, ConflictManager) {
    let conflicts: Vec<ConflictV2> = modpack.mods
        .iter()
        .flat_map(|curr_dir| {
            let curr_files: Vec<&ModFile> = curr_dir.files.iter().collect();

            // Check for conflict
            modpack.mods
            .iter()
            .filter(move |dir| *dir != curr_dir) // Make sure we don't process the current directory
            .filter(move |dir| dir.files.iter().any(|file| curr_files.contains(&file))) // Only keep the directories that are conflicting 
            .map(move |conflict| {
                ConflictV2::new(curr_dir.clone(), conflict.clone())
            })
        }).collect();

        // Remove all of the mods that are conflicting from the Modpack
        modpack.mods.drain_filter(|mods| {
            conflicts.iter().any(|conflict| conflict.first == *mods || conflict.second == *mods)
        });

        (UnconflictingModpack(modpack), conflicts.into())
}

#[derive(Debug, Default, Clone, PartialEq, Hash, Eq, Serialize)]
pub struct ModFile {
    pub path: Utf8PathBuf,
    pub size: u64,
}

#[derive(Debug, Default, PartialEq, Hash, Eq, Serialize)]
pub struct Conflict {
    #[serde(rename = "Conflicting mod")]
    conflicting_mod: Utf8PathBuf,
    #[serde(rename = "Conflicting with")]
    conflict_with: Utf8PathBuf,
}

pub struct ConflictV2 {
    pub first: ModDir,
    pub second: ModDir,
}

impl ConflictV2 {
    pub fn new(first: ModDir, second: ModDir) -> Self {
        Self { first, second }
    }
}

impl From<Vec<ConflictV2> > for ConflictManager {
    fn from(vec: Vec<ConflictV2>) -> Self {
        Self(vec)
    }
}

pub struct ConflictManager(Vec<ConflictV2>);

impl ConflictManager {
    pub fn rebase(&mut self, dir: &ModDir) {
        self.0.drain_filter(|conflict| conflict.first == *dir || conflict.second == *dir);
    }

    pub fn next(&mut self) -> Option<ConflictV2> {
        self.0.pop()
    }
}

pub struct MsbtHandler;

impl ExtensionHandler for MsbtHandler {
    fn patch_file<B: AsRef<[u8]>>(&self, _buffer: B) -> Vec<u8> {
        todo!()
    }
}

pub trait ExtensionHandler {
    fn patch_file<B: AsRef<[u8]>>(&self, buffer: B) -> Vec<u8>;
}

pub fn acquire_extension_handler<H: Into<Hash40>>(extension: H) -> Option<()> {
    match extension.into() {
        _ => None,
    }
}

// enum ModFileSource {
//     Api,
//     Mod,
//     Cache,
// }

// impl ModFileSource {
//     pub fn get_file(&self) -> Vec<u8> {
//         Vec::new()
//     }
// }

// // Adding placeholder functions here until the backend for it is written
// pub fn get_modded_file(path: &Path) -> Vec<u8> {
//     // Acquire from source
//     let source = acquire_source(path);

//     // Check if this source allows for patching, as Cached files should already be patched by now
//     if can_be_patched(&source) {
//         match acquire_extension_handler(Path::new("xmsbt")) {
//             Some(_) => todo!(),
//             None => todo!(),
//         }
//         source.get_file()
//     } else {
//         source.get_file()
//     }
// }

// pub fn acquire_source(path: &Path) -> ModFileSource {
//     // Placeholder
//     ModFileSource::Mod
// }

// pub fn can_be_patched(source: &ModFileSource) -> bool {
//     match source {
//         ModFileSource::Cache => false,
//         _ => true
//     }
// }
