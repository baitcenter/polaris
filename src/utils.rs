use app_dirs::{app_root, AppDataType, AppInfo};
use error_chain::bail;
use std::fs;
use std::path::{Path, PathBuf};

use crate::errors::*;

#[cfg(not(target_os = "linux"))]
const APP_INFO: AppInfo = AppInfo {
	name: "Polaris",
	author: "Permafrost",
};

#[cfg(target_os = "linux")]
const APP_INFO: AppInfo = AppInfo {
	name: "polaris",
	author: "permafrost",
};

pub fn get_data_root() -> Result<PathBuf> {
	if let Ok(root) = app_root(AppDataType::UserData, &APP_INFO) {
		fs::create_dir_all(&root).chain_err(|| format!("opening user data: {}", root.display()))?;
		return Ok(root);
	}
	bail!("Could not retrieve data directory root");
}

#[derive(Debug, PartialEq)]
pub enum AudioFormat {
	FLAC,
	MP3,
	MP4,
	MPC,
	OGG,
}

pub fn get_audio_format(path: &Path) -> Option<AudioFormat> {
	let extension = match path.extension() {
		Some(e) => e,
		_ => return None,
	};
	let extension = match extension.to_str() {
		Some(e) => e,
		_ => return None,
	};
	match extension.to_lowercase().as_str() {
		"flac" => Some(AudioFormat::FLAC),
		"mp3" => Some(AudioFormat::MP3),
		"m4a" => Some(AudioFormat::MP4),
		"mpc" => Some(AudioFormat::MPC),
		"ogg" => Some(AudioFormat::OGG),
		_ => None,
	}
}

#[test]
fn test_get_audio_format() {
	assert_eq!(get_audio_format(Path::new("animals/🐷/my🐖file.jpg")), None);
	assert_eq!(
		get_audio_format(Path::new("animals/🐷/my🐖file.flac")),
		Some(AudioFormat::FLAC)
	);
}

pub fn is_image(path: &Path) -> bool {
	let extension = match path.extension() {
		Some(e) => e,
		_ => return false,
	};
	let extension = match extension.to_str() {
		Some(e) => e,
		_ => return false,
	};
	match extension.to_lowercase().as_str() {
		"png" => true,
		"gif" => true,
		"jpg" => true,
		"jpeg" => true,
		"bmp" => true,
		_ => false,
	}
}

#[test]
fn test_is_image() {
	assert!(!is_image(Path::new("animals/🐷/my🐖file.mp3")));
	assert!(is_image(Path::new("animals/🐷/my🐖file.jpg")));
}
