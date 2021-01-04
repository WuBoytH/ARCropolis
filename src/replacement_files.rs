use std::{fs, io, slice};
use std::fs::DirEntry;
use std::path::PathBuf;
use std::collections::HashMap;
use std::sync::atomic::{ AtomicBool, Ordering };

use crate::{config::CONFIG, runtime};

use owo_colors::OwoColorize;

use smash_arc::{
    Hash40,
    FileData,
    ArcLookup,
};

use runtime::{ LoadedTables, ResServiceState };

use log::{ info, warn };

type ArcCallback = extern "C" fn(Hash40, *mut skyline::libc::c_void, usize) -> bool;

lazy_static::lazy_static! {
    pub static ref ARC_FILES: parking_lot::RwLock<ArcFiles> = parking_lot::RwLock::new(ArcFiles::new());
    pub static ref ARC_CALLBACKS: parking_lot::RwLock<HashMap<Hash40, ArcCallback>> = parking_lot::RwLock::new(HashMap::new());
    pub static ref CB_QUEUE: parking_lot::RwLock<HashMap<Hash40, FileCtx>> = parking_lot::RwLock::new(HashMap::new());
}

pub static QUEUE_HANDLED: AtomicBool = AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn subscribe_callback(hash: Hash40, extension: *const u8, extension_len: usize, callback: ArcCallback) {
    unsafe {
        let filepath = format!("rom:/virtual.{}", std::str::from_utf8(slice::from_raw_parts(extension, extension_len)).unwrap());
        let path = std::path::Path::new(&filepath).to_path_buf();

        let file_ctx = FileCtx {
            hash, path, virtual_file:true,
            .. FileCtx::new()
        };

        if QUEUE_HANDLED.load(Ordering::SeqCst) == true {
            ARC_FILES.write().0.insert(hash, file_ctx);
        } else {
            CB_QUEUE.write().insert(hash, file_ctx);
        }
    
        ARC_CALLBACKS.write().insert(hash, callback);
    }
}

#[no_mangle]
pub extern "C" fn subscribe_callback_with_size(hash: Hash40, filesize: u32, extension: *const u8, extension_len: usize, callback: ArcCallback) {
    unsafe {
        let filepath = format!("rom:/virtual.{}", std::str::from_utf8(slice::from_raw_parts(extension, extension_len)).unwrap());
        let path = std::path::Path::new(&filepath).to_path_buf();

        let mut file_ctx = FileCtx {
            hash, filesize, path, virtual_file:true,
            .. FileCtx::new()
        };

        if QUEUE_HANDLED.load(Ordering::SeqCst) == true {
            file_ctx.filesize_replacement();
            ARC_FILES.write().0.insert(hash, file_ctx);
        } else {
            CB_QUEUE.write().insert(hash, file_ctx);
        }
    
        ARC_CALLBACKS.write().insert(hash, callback);
    }
}

#[no_mangle]
pub extern "C" fn scan_path(path: *const u8, path_len: usize, umm: bool) {
    unsafe {
        let path = std::str::from_utf8(slice::from_raw_parts(path, path_len)).unwrap();
        let path = std::path::Path::new(&path).to_path_buf();

        if umm {
            ARC_FILES.write().visit_umm_dirs(&path).unwrap();
        } else {
            ARC_FILES.write().visit_dir(&path, path_len).unwrap();
        }
    }
}

pub struct ArcFiles(pub HashMap<Hash40, FileCtx>);

#[derive(Debug, Clone)]
pub struct FileCtx {
    pub path: PathBuf,
    pub hash: Hash40,
    pub filesize: u32,
    pub virtual_file: bool,
    pub orig_subfile: smash_arc::FileData,
}

#[macro_export]
macro_rules! get_from_hash {
    ($hash:expr) => {
        parking_lot::RwLockReadGuard::try_map(
            $crate::replacement_files::ARC_FILES.read(),
            |x| x.get($hash)
        )
    };
}

impl ArcFiles {
    fn new() -> Self {
        let mut instance = Self(HashMap::new());

        let _ = instance.visit_dir(&PathBuf::from(&CONFIG.paths.arc), CONFIG.paths.arc.len());
        let _ = instance.visit_umm_dirs(&PathBuf::from(&CONFIG.paths.umm));

        if let Some(extra_paths) = &CONFIG.paths.extra_paths {
            for path in extra_paths {
                let _ = instance.visit_umm_dirs(&PathBuf::from(path));
            }
        }

        instance
    }

    pub fn get<H: Into<Hash40>>(&self, hash: H) -> Option<&FileCtx> {
        self.0.get(&hash.into())
    }

    /// Visit Ultimate Mod Manager directories for backwards compatibility
    fn visit_umm_dirs(&mut self, dir: &PathBuf) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;

            // Skip any directory starting with a period
            if entry.file_name().to_str().unwrap().starts_with(".") {
                continue;
            }

            let path = PathBuf::from(&format!("{}/{}", dir.display(), entry.path().display()));

            if path.is_dir() {
                self.visit_dir(&path, path.to_str().unwrap().len())?;
            }
        }

        Ok(())
    }

    fn visit_dir(&mut self, dir: &PathBuf, arc_dir_len: usize) -> io::Result<()> {
        fs::read_dir(dir)?
            .map(|entry| {
                let entry = entry?;
                let path = PathBuf::from(&format!("{}/{}", dir.display(), entry.path().display()));

                // Check if the entry is a directory or a file
                if entry.file_type().unwrap().is_dir() {
                    // If it is one of the stream randomizer directories
                    if let Some(_) = path.extension() {
                        match self.visit_file(&entry, &path, arc_dir_len) {
                            Ok(file_ctx) => {
                                self.0.insert(file_ctx.hash, file_ctx);
                                return Ok(());
                            }
                            Err(err) => {
                                warn!("{}", err);
                                return Ok(());
                            }
                        }
                    }

                    // If not, treat it as a regular directory
                    self.visit_dir(&path, arc_dir_len).unwrap();
                } else {
                    match self.visit_file(&entry, &path, arc_dir_len) {
                        Ok(file_ctx) => {
                            if let Some(ctx) = self.0.get_mut(&file_ctx.hash) {
                                ctx.filesize = file_ctx.filesize;
                            } else {
                                self.0.insert(file_ctx.hash, file_ctx);
                            }
                            return Ok(());
                        }
                        Err(err) => {
                            warn!("{}", err);
                            return Ok(());
                        }
                    }
                }

                Ok(())
            })
            .collect()
    }

    fn visit_file(
        &self,
        entry: &DirEntry,
        full_path: &PathBuf,
        arc_dir_len: usize,
    ) -> Result<FileCtx, String> {
        // Skip any file starting with a period, to avoid any error related to path.extension()
        if entry.file_name().to_str().unwrap().starts_with(".") {
            return Err(format!("[ARC::Discovery] File '{}' starts with a period, skipping", full_path.display().bright_yellow()));
        }

        // Make sure the file has an extension to not cause issues with the code that follows
        match full_path.extension() {
            Some(_) => {}
            None => return Err(format!("[ARC::Discovery] File '{}' does not have an extension, skipping", full_path.display().bright_yellow())),
        }

        // This is the path that gets hashed. Replace ; to : for Smash's internal paths since ; is not a valid character for filepaths.
        let mut game_path = full_path.to_str().unwrap()[arc_dir_len + 1..].replace(";", ":");

        // Remove the regional indicator
        if let Some(regional_marker) = game_path.find("+") {
            game_path.replace_range(regional_marker..game_path.find(".").unwrap(), "");
        }

        // TODO: Move that stuff in a separate function that can handle more than one format
        match game_path.strip_suffix("mp4") {
            Some(x) => game_path = format!("{}{}", x, "webm"),
            None => (),
        }

        let mut file_ctx = FileCtx::new();

        file_ctx.path = full_path.to_path_buf();
        file_ctx.hash = Hash40::from(game_path.as_str());

        file_ctx.filesize = match entry.metadata() {
            Ok(meta) => meta.len() as u32,
            Err(err) => panic!(err),
        };

        // Don't bother if the region doesn't match
        if file_ctx.get_region() != ResServiceState::get_instance().regular_region_idx {
            return Err(format!("[ARC::Discovery] File '{}' does not have a matching region, skipping", file_ctx.path.display().bright_yellow()));
        }

        //file_ctx.filesize_replacement();
        Ok(file_ctx)
    }
}

impl FileCtx {
    pub fn new() -> Self {
        FileCtx {
            path: PathBuf::new(),
            hash: Hash40(0),
            filesize: 0,
            virtual_file: false,
            orig_subfile: smash_arc::FileData {
                offset_in_folder: 0,
                comp_size: 0,
                decomp_size: 0,
                flags: smash_arc::FileDataFlags::new().with_compressed(false).with_use_zstd(false).with_unk(0),
            },
        }
    }

    pub fn get_region(&self) -> u32 {
        // Default to the player's region index
        let mut region_index = ResServiceState::get_instance().regular_region_idx;

        // Make sure the file has an extension
        if let Some(_) = self.path.extension() {
            // Split the region identifier from the filepath
            let region = self.path.file_name().unwrap().to_str().unwrap().to_string();
            // Check if the filepath it contains a + symbol
            if let Some(region_marker) = region.find('+') {
                match &region[region_marker + 1..region_marker + 6] {
                    "jp_ja" => region_index = 0,
                    "us_en" => region_index = 1,
                    "us_fr" => region_index = 2,
                    "us_es" => region_index = 3,
                    "eu_en" => region_index = 4,
                    "eu_fr" => region_index = 5,
                    "eu_es" => region_index = 6,
                    "eu_de" => region_index = 7,
                    "eu_nl" => region_index = 8,
                    "eu_it" => region_index = 9,
                    "eu_ru" => region_index = 10,
                    "kr_ko" => region_index = 11,
                    "zh_cn" => region_index = 12,
                    "zh_tw" => region_index = 13,
                    _ => region_index = 1,
                }
            }
        }

        region_index
    }

    pub fn get_subfile(&self) -> &mut smash_arc::FileData {
        let loaded_arc = LoadedTables::get_instance().get_arc();

        let file_info = loaded_arc.get_file_info_from_hash(self.hash).unwrap();

        unsafe {
            let file_data = (loaded_arc.get_file_data(file_info) as *const FileData) as *mut FileData;
            &mut *file_data
        }
    }

    pub fn get_file_content(&self) -> Vec<u8> {
        // TODO: Add error handling in case the user deleted the file while running
        fs::read(&self.path).unwrap()
    }

    pub fn filesize_replacement(&mut self) {
        let loaded_tables = LoadedTables::get_instance();

            match loaded_tables.get_arc().get_file_data_from_hash(self.hash) {
                Ok(_) => {},
                Err(_) => {
                    println!("[ARC::Patching] File '{}' does not have a hash found in FileInfoPath, skipping",self.path.display());
                    return;
                },
            }

            // Backup the Subfile for when file watching is added
            self.orig_subfile = self.get_subfile().clone();

            let mut subfile = self.get_subfile();

        //info!("[ARC::Patching] File '{}', decomp size: {:x}",self.path.display().bright_yellow(),subfile.decomp_size.cyan());

           if subfile.decomp_size < self.filesize {
                 subfile.decomp_size = self.filesize;
                info!("[ARC::Patching] File '{}' has a new patched decompressed size: {:#x}",self.path.display().bright_yellow(),subfile.decomp_size.bright_red());
            }
    }
}
