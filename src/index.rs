use core::ops::Deref;
use diesel;
use diesel::dsl::sql;
use diesel::prelude::*;
use diesel::sql_types;
use diesel::sqlite::SqliteConnection;
use error_chain::bail;
use log::{error, info};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::mpsc::*;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time;

use crate::config::MiscSettings;
#[cfg(test)]
use crate::db;
use crate::db::{directories, misc_settings, songs};
use crate::db::{ConnectionSource, DB};
use crate::errors;
use crate::metadata;
use crate::vfs::{VFSSource, VFS};

const INDEX_BUILDING_INSERT_BUFFER_SIZE: usize = 1000; // Insertions in each transaction
const INDEX_BUILDING_CLEAN_BUFFER_SIZE: usize = 500; // Insertions in each transaction

no_arg_sql_function!(
	random,
	sql_types::Integer,
	"Represents the SQL RANDOM() function"
);

enum Command {
	REINDEX,
	EXIT,
}

struct CommandReceiver {
	receiver: Receiver<Command>,
}

impl CommandReceiver {
	fn new(receiver: Receiver<Command>) -> CommandReceiver {
		CommandReceiver { receiver }
	}
}

pub struct CommandSender {
	sender: Mutex<Sender<Command>>,
}

impl CommandSender {
	fn new(sender: Sender<Command>) -> CommandSender {
		CommandSender {
			sender: Mutex::new(sender),
		}
	}

	pub fn trigger_reindex(&self) -> Result<(), errors::Error> {
		let sender = self.sender.lock().unwrap();
		match sender.send(Command::REINDEX) {
			Ok(_) => Ok(()),
			Err(_) => bail!("Trigger reindex channel error"),
		}
	}

	#[allow(dead_code)]
	pub fn exit(&self) -> Result<(), errors::Error> {
		let sender = self.sender.lock().unwrap();
		match sender.send(Command::EXIT) {
			Ok(_) => Ok(()),
			Err(_) => bail!("Index exit channel error"),
		}
	}
}

pub fn init(db: Arc<DB>) -> Arc<CommandSender> {
	let (index_sender, index_receiver) = channel();
	let command_sender = Arc::new(CommandSender::new(index_sender));
	let command_receiver = CommandReceiver::new(index_receiver);

	// Start update loop
	let db_ref = db.clone();
	std::thread::spawn(move || {
		let db = db_ref.deref();
		update_loop(db, &command_receiver);
	});

	command_sender
}

#[derive(Debug, PartialEq, Queryable, QueryableByName, Serialize, Deserialize)]
#[table_name = "songs"]
pub struct Song {
	#[serde(skip_serializing, skip_deserializing)]
	id: i32,
	pub path: String,
	#[serde(skip_serializing, skip_deserializing)]
	pub parent: String,
	pub track_number: Option<i32>,
	pub disc_number: Option<i32>,
	pub title: Option<String>,
	pub artist: Option<String>,
	pub album_artist: Option<String>,
	pub year: Option<i32>,
	pub album: Option<String>,
	pub artwork: Option<String>,
	pub duration: Option<i32>,
}

#[derive(Debug, PartialEq, Queryable, Serialize, Deserialize)]
pub struct Directory {
	#[serde(skip_serializing, skip_deserializing)]
	id: i32,
	pub path: String,
	#[serde(skip_serializing, skip_deserializing)]
	pub parent: Option<String>,
	pub artist: Option<String>,
	pub year: Option<i32>,
	pub album: Option<String>,
	pub artwork: Option<String>,
	pub date_added: i32,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum CollectionFile {
	Directory(Directory),
	Song(Song),
}

#[derive(Debug, Insertable)]
#[table_name = "songs"]
struct NewSong {
	path: String,
	parent: String,
	track_number: Option<i32>,
	disc_number: Option<i32>,
	title: Option<String>,
	artist: Option<String>,
	album_artist: Option<String>,
	year: Option<i32>,
	album: Option<String>,
	artwork: Option<String>,
	duration: Option<i32>,
}

#[derive(Debug, Insertable)]
#[table_name = "directories"]
struct NewDirectory {
	path: String,
	parent: Option<String>,
	artist: Option<String>,
	year: Option<i32>,
	album: Option<String>,
	artwork: Option<String>,
	date_added: i32,
}

struct IndexBuilder<'conn> {
	new_songs: Vec<NewSong>,
	new_directories: Vec<NewDirectory>,
	connection: &'conn Mutex<SqliteConnection>,
	album_art_pattern: Regex,
}

impl<'conn> IndexBuilder<'conn> {
	fn new(
		connection: &Mutex<SqliteConnection>,
		album_art_pattern: Regex,
	) -> Result<IndexBuilder<'_>, errors::Error> {
		let mut new_songs = Vec::new();
		let mut new_directories = Vec::new();
		new_songs.reserve_exact(INDEX_BUILDING_INSERT_BUFFER_SIZE);
		new_directories.reserve_exact(INDEX_BUILDING_INSERT_BUFFER_SIZE);
		Ok(IndexBuilder {
			new_songs,
			new_directories,
			connection,
			album_art_pattern,
		})
	}

	fn flush_songs(&mut self) -> Result<(), errors::Error> {
		let connection = self.connection.lock().unwrap();
		let connection = connection.deref();
		connection.transaction::<_, errors::Error, _>(|| {
			diesel::insert_into(songs::table)
				.values(&self.new_songs)
				.execute(connection)?;
			Ok(())
		})?;
		self.new_songs.clear();
		Ok(())
	}

	fn flush_directories(&mut self) -> Result<(), errors::Error> {
		let connection = self.connection.lock().unwrap();
		let connection = connection.deref();
		connection.transaction::<_, errors::Error, _>(|| {
			diesel::insert_into(directories::table)
				.values(&self.new_directories)
				.execute(connection)?;
			Ok(())
		})?;
		self.new_directories.clear();
		Ok(())
	}

	fn push_song(&mut self, song: NewSong) -> Result<(), errors::Error> {
		if self.new_songs.len() >= self.new_songs.capacity() {
			self.flush_songs()?;
		}
		self.new_songs.push(song);
		Ok(())
	}

	fn push_directory(&mut self, directory: NewDirectory) -> Result<(), errors::Error> {
		if self.new_directories.len() >= self.new_directories.capacity() {
			self.flush_directories()?;
		}
		self.new_directories.push(directory);
		Ok(())
	}

	fn get_artwork(&self, dir: &Path) -> Result<Option<String>, errors::Error> {
		for file in fs::read_dir(dir)? {
			let file = file?;
			if let Some(name_string) = file.file_name().to_str() {
				if self.album_art_pattern.is_match(name_string) {
					return Ok(file.path().to_str().map(|p| p.to_owned()));
				}
			}
		}
		Ok(None)
	}

	fn populate_directory(
		&mut self,
		parent: Option<&Path>,
		path: &Path,
	) -> Result<(), errors::Error> {
		// Find artwork
		let artwork = self.get_artwork(path).unwrap_or(None);

		// Extract path and parent path
		let parent_string = parent.and_then(|p| p.to_str()).map(|s| s.to_owned());
		let path_string = path.to_str().ok_or("Invalid directory path")?;

		// Find date added
		let metadata = fs::metadata(path_string)?;
		let created = metadata
			.created()
			.or_else(|_| metadata.modified())?
			.duration_since(time::UNIX_EPOCH)?
			.as_secs() as i32;

		let mut directory_album = None;
		let mut directory_year = None;
		let mut directory_artist = None;
		let mut inconsistent_directory_album = false;
		let mut inconsistent_directory_year = false;
		let mut inconsistent_directory_artist = false;

		// Sub directories
		let mut sub_directories = Vec::new();

		// Insert content
		for file in fs::read_dir(path)? {
			let file_path = match file {
				Ok(f) => f.path(),
				_ => {
					error!("File read error within {}", path_string);
					break;
				}
			};

			if file_path.is_dir() {
				sub_directories.push(file_path.to_path_buf());
				continue;
			}

			if let Some(file_path_string) = file_path.to_str() {
				if let Ok(tags) = metadata::read(file_path.as_path()) {
					if tags.year.is_some() {
						inconsistent_directory_year |=
							directory_year.is_some() && directory_year != tags.year;
						directory_year = tags.year;
					}

					if tags.album.is_some() {
						inconsistent_directory_album |=
							directory_album.is_some() && directory_album != tags.album;
						directory_album = tags.album.as_ref().cloned();
					}

					if tags.album_artist.is_some() {
						inconsistent_directory_artist |=
							directory_artist.is_some() && directory_artist != tags.album_artist;
						directory_artist = tags.album_artist.as_ref().cloned();
					} else if tags.artist.is_some() {
						inconsistent_directory_artist |=
							directory_artist.is_some() && directory_artist != tags.artist;
						directory_artist = tags.artist.as_ref().cloned();
					}

					let song = NewSong {
						path: file_path_string.to_owned(),
						parent: path_string.to_owned(),
						disc_number: tags.disc_number.map(|n| n as i32),
						track_number: tags.track_number.map(|n| n as i32),
						title: tags.title,
						duration: tags.duration.map(|n| n as i32),
						artist: tags.artist,
						album_artist: tags.album_artist,
						album: tags.album,
						year: tags.year,
						artwork: artwork.as_ref().cloned(),
					};

					self.push_song(song)?;
				}
			}
		}

		// Insert directory
		if inconsistent_directory_year {
			directory_year = None;
		}
		if inconsistent_directory_album {
			directory_album = None;
		}
		if inconsistent_directory_artist {
			directory_artist = None;
		}

		let directory = NewDirectory {
			path: path_string.to_owned(),
			parent: parent_string,
			artwork,
			album: directory_album,
			artist: directory_artist,
			year: directory_year,
			date_added: created,
		};
		self.push_directory(directory)?;

		// Populate subdirectories
		for sub_directory in sub_directories {
			self.populate_directory(Some(path), &sub_directory)?;
		}

		Ok(())
	}
}

fn clean<T>(db: &T) -> Result<(), errors::Error>
where
	T: ConnectionSource + VFSSource,
{
	let vfs = db.get_vfs()?;

	{
		let all_songs: Vec<String>;
		{
			let connection = db.get_connection();
			all_songs = songs::table.select(songs::path).load(connection.deref())?;
		}

		let missing_songs = all_songs
			.into_iter()
			.filter(|ref song_path| {
				let path = Path::new(&song_path);
				!path.exists() || vfs.real_to_virtual(path).is_err()
			})
			.collect::<Vec<_>>();

		{
			let connection = db.get_connection();
			for chunk in missing_songs[..].chunks(INDEX_BUILDING_CLEAN_BUFFER_SIZE) {
				diesel::delete(songs::table.filter(songs::path.eq_any(chunk)))
					.execute(connection.deref())?;
			}
		}
	}

	{
		let all_directories: Vec<String>;
		{
			let connection = db.get_connection();
			all_directories = directories::table
				.select(directories::path)
				.load(connection.deref())?;
		}

		let missing_directories = all_directories
			.into_iter()
			.filter(|ref directory_path| {
				let path = Path::new(&directory_path);
				!path.exists() || vfs.real_to_virtual(path).is_err()
			})
			.collect::<Vec<_>>();

		{
			let connection = db.get_connection();
			for chunk in missing_directories[..].chunks(INDEX_BUILDING_CLEAN_BUFFER_SIZE) {
				diesel::delete(directories::table.filter(directories::path.eq_any(chunk)))
					.execute(connection.deref())?;
			}
		}
	}

	Ok(())
}

fn populate<T>(db: &T) -> Result<(), errors::Error>
where
	T: ConnectionSource + VFSSource,
{
	let vfs = db.get_vfs()?;
	let mount_points = vfs.get_mount_points();

	let album_art_pattern;
	{
		let connection = db.get_connection();
		let settings: MiscSettings = misc_settings::table.get_result(connection.deref())?;
		album_art_pattern = Regex::new(&settings.index_album_art_pattern)?;
	}

	let connection_mutex = db.get_connection_mutex();
	let mut builder = IndexBuilder::new(connection_mutex.deref(), album_art_pattern)?;
	for target in mount_points.values() {
		builder.populate_directory(None, target.as_path())?;
	}
	builder.flush_songs()?;
	builder.flush_directories()?;
	Ok(())
}

pub fn update<T>(db: &T) -> Result<(), errors::Error>
where
	T: ConnectionSource + VFSSource,
{
	let start = time::Instant::now();
	info!("Beginning library index update");
	clean(db)?;
	populate(db)?;
	info!(
		"Library index update took {} seconds",
		start.elapsed().as_secs()
	);
	Ok(())
}

fn update_loop<T>(db: &T, command_buffer: &CommandReceiver)
where
	T: ConnectionSource + VFSSource,
{
	loop {
		// Wait for a command
		if command_buffer.receiver.recv().is_err() {
			return;
		}

		// Flush the buffer to ignore spammy requests
		loop {
			match command_buffer.receiver.try_recv() {
				Err(TryRecvError::Disconnected) => return,
				Ok(Command::EXIT) => return,
				Err(TryRecvError::Empty) => break,
				Ok(_) => (),
			}
		}

		// Do the update
		if let Err(e) = update(db) {
			error!("Error while updating index: {}", e);
		}
	}
}

pub fn self_trigger<T>(db: &T, command_buffer: &Arc<CommandSender>)
where
	T: ConnectionSource,
{
	loop {
		{
			let command_buffer = command_buffer.deref();
			if let Err(e) = command_buffer.trigger_reindex() {
				error!("Error while writing to index command buffer: {}", e);
				return;
			}
		}
		let sleep_duration;
		{
			let connection = db.get_connection();
			let settings: Result<MiscSettings, errors::Error> = misc_settings::table
				.get_result(connection.deref())
				.map_err(|e| e.into());
			if let Err(ref e) = settings {
				error!("Could not retrieve index sleep duration: {}", e);
			}
			sleep_duration = settings
				.map(|s| s.index_sleep_duration_seconds)
				.unwrap_or(1800);
		}
		thread::sleep(time::Duration::from_secs(sleep_duration as u64));
	}
}

pub fn virtualize_song(vfs: &VFS, mut song: Song) -> Option<Song> {
	song.path = match vfs.real_to_virtual(Path::new(&song.path)) {
		Ok(p) => p.to_string_lossy().into_owned(),
		_ => return None,
	};
	if let Some(artwork_path) = song.artwork {
		song.artwork = match vfs.real_to_virtual(Path::new(&artwork_path)) {
			Ok(p) => Some(p.to_string_lossy().into_owned()),
			_ => None,
		};
	}
	Some(song)
}

fn virtualize_directory(vfs: &VFS, mut directory: Directory) -> Option<Directory> {
	directory.path = match vfs.real_to_virtual(Path::new(&directory.path)) {
		Ok(p) => p.to_string_lossy().into_owned(),
		_ => return None,
	};
	if let Some(artwork_path) = directory.artwork {
		directory.artwork = match vfs.real_to_virtual(Path::new(&artwork_path)) {
			Ok(p) => Some(p.to_string_lossy().into_owned()),
			_ => None,
		};
	}
	Some(directory)
}

pub fn browse<T, P>(db: &T, virtual_path: P) -> Result<Vec<CollectionFile>, errors::Error>
where
	T: ConnectionSource + VFSSource,
	P: AsRef<Path>,
{
	let mut output = Vec::new();
	let vfs = db.get_vfs()?;
	let connection = db.get_connection();

	if virtual_path.as_ref().components().count() == 0 {
		// Browse top-level
		let real_directories: Vec<Directory> = directories::table
			.filter(directories::parent.is_null())
			.load(connection.deref())?;
		let virtual_directories = real_directories
			.into_iter()
			.filter_map(|s| virtualize_directory(&vfs, s));
		output.extend(virtual_directories.map(CollectionFile::Directory));
	} else {
		// Browse sub-directory
		let real_path = vfs.virtual_to_real(virtual_path)?;
		let real_path_string = real_path.as_path().to_string_lossy().into_owned();

		let real_directories: Vec<Directory> = directories::table
			.filter(directories::parent.eq(&real_path_string))
			.order(sql::<sql_types::Bool>("path COLLATE NOCASE ASC"))
			.load(connection.deref())?;
		let virtual_directories = real_directories
			.into_iter()
			.filter_map(|s| virtualize_directory(&vfs, s));
		output.extend(virtual_directories.map(CollectionFile::Directory));

		let real_songs: Vec<Song> = songs::table
			.filter(songs::parent.eq(&real_path_string))
			.order(sql::<sql_types::Bool>("path COLLATE NOCASE ASC"))
			.load(connection.deref())?;
		let virtual_songs = real_songs
			.into_iter()
			.filter_map(|s| virtualize_song(&vfs, s));
		output.extend(virtual_songs.map(CollectionFile::Song));
	}

	Ok(output)
}

pub fn flatten<T, P>(db: &T, virtual_path: P) -> Result<Vec<Song>, errors::Error>
where
	T: ConnectionSource + VFSSource,
	P: AsRef<Path>,
{
	use self::songs::dsl::*;
	let vfs = db.get_vfs()?;
	let connection = db.get_connection();

	let real_songs: Vec<Song> = if virtual_path.as_ref().parent() != None {
		let real_path = vfs.virtual_to_real(virtual_path)?;
		let like_path = real_path.as_path().to_string_lossy().into_owned() + "%";
		songs
			.filter(path.like(&like_path))
			.order(path)
			.load(connection.deref())?
	} else {
		songs.order(path).load(connection.deref())?
	};

	let virtual_songs = real_songs
		.into_iter()
		.filter_map(|s| virtualize_song(&vfs, s));
	Ok(virtual_songs.collect::<Vec<_>>())
}

pub fn get_random_albums<T>(db: &T, count: i64) -> Result<Vec<Directory>, errors::Error>
where
	T: ConnectionSource + VFSSource,
{
	use self::directories::dsl::*;
	let vfs = db.get_vfs()?;
	let connection = db.get_connection();
	let real_directories = directories
		.filter(album.is_not_null())
		.limit(count)
		.order(random)
		.load(connection.deref())?;
	let virtual_directories = real_directories
		.into_iter()
		.filter_map(|s| virtualize_directory(&vfs, s));
	Ok(virtual_directories.collect::<Vec<_>>())
}

pub fn get_recent_albums<T>(db: &T, count: i64) -> Result<Vec<Directory>, errors::Error>
where
	T: ConnectionSource + VFSSource,
{
	use self::directories::dsl::*;
	let vfs = db.get_vfs()?;
	let connection = db.get_connection();
	let real_directories: Vec<Directory> = directories
		.filter(album.is_not_null())
		.order(date_added.desc())
		.limit(count)
		.load(connection.deref())?;
	let virtual_directories = real_directories
		.into_iter()
		.filter_map(|s| virtualize_directory(&vfs, s));
	Ok(virtual_directories.collect::<Vec<_>>())
}

pub fn search<T>(db: &T, query: &str) -> Result<Vec<CollectionFile>, errors::Error>
where
	T: ConnectionSource + VFSSource,
{
	let vfs = db.get_vfs()?;
	let connection = db.get_connection();
	let like_test = format!("%{}%", query);
	let mut output = Vec::new();

	// Find dirs with matching path and parent not matching
	{
		use self::directories::dsl::*;
		let real_directories: Vec<Directory> = directories
			.filter(path.like(&like_test))
			.filter(parent.not_like(&like_test))
			.load(connection.deref())?;

		let virtual_directories = real_directories
			.into_iter()
			.filter_map(|s| virtualize_directory(&vfs, s));

		output.extend(virtual_directories.map(CollectionFile::Directory));
	}

	// Find songs with matching title/album/artist and non-matching parent
	{
		use self::songs::dsl::*;
		let real_songs: Vec<Song> = songs
			.filter(
				path.like(&like_test)
					.or(title.like(&like_test))
					.or(album.like(&like_test))
					.or(artist.like(&like_test))
					.or(album_artist.like(&like_test)),
			)
			.filter(parent.not_like(&like_test))
			.load(connection.deref())?;

		let virtual_songs = real_songs
			.into_iter()
			.filter_map(|s| virtualize_song(&vfs, s));

		output.extend(virtual_songs.map(CollectionFile::Song));
	}

	Ok(output)
}

pub fn get_song<T>(db: &T, virtual_path: &Path) -> Result<Song, errors::Error>
where
	T: ConnectionSource + VFSSource,
{
	let vfs = db.get_vfs()?;
	let connection = db.get_connection();
	let real_path = vfs.virtual_to_real(virtual_path)?;
	let real_path_string = real_path.as_path().to_string_lossy();

	use self::songs::dsl::*;
	let real_song: Song = songs
		.filter(path.eq(real_path_string))
		.get_result(connection.deref())?;

	match virtualize_song(&vfs, real_song) {
		Some(s) => Ok(s),
		_ => bail!("Missing VFS mapping"),
	}
}

#[test]
fn test_populate() {
	let db = db::_get_test_db("populate.sqlite");
	update(&db).unwrap();
	update(&db).unwrap(); // Check that subsequent updates don't run into conflicts

	let connection = db.get_connection();
	let all_directories: Vec<Directory> = directories::table.load(connection.deref()).unwrap();
	let all_songs: Vec<Song> = songs::table.load(connection.deref()).unwrap();
	assert_eq!(all_directories.len(), 5);
	assert_eq!(all_songs.len(), 12);
}

#[test]
fn test_metadata() {
	let mut target = PathBuf::new();
	target.push("test");
	target.push("collection");
	target.push("Tobokegao");
	target.push("Picnic");

	let mut song_path = target.clone();
	song_path.push("05 - シャーベット (Sherbet).mp3");

	let mut artwork_path = target.clone();
	artwork_path.push("Folder.png");

	let db = db::_get_test_db("metadata.sqlite");
	update(&db).unwrap();

	let connection = db.get_connection();
	let songs: Vec<Song> = songs::table
		.filter(songs::title.eq("シャーベット (Sherbet)"))
		.load(connection.deref())
		.unwrap();

	assert_eq!(songs.len(), 1);
	let song = &songs[0];
	assert_eq!(song.path, song_path.to_string_lossy().as_ref());
	assert_eq!(song.track_number, Some(5));
	assert_eq!(song.disc_number, None);
	assert_eq!(song.title, Some("シャーベット (Sherbet)".to_owned()));
	assert_eq!(song.artist, Some("Tobokegao".to_owned()));
	assert_eq!(song.album_artist, None);
	assert_eq!(song.album, Some("Picnic".to_owned()));
	assert_eq!(song.year, Some(2016));
	assert_eq!(
		song.artwork,
		Some(artwork_path.to_string_lossy().into_owned())
	);
}

#[test]
fn test_browse_top_level() {
	let mut root_path = PathBuf::new();
	root_path.push("root");

	let db = db::_get_test_db("browse_top_level.sqlite");
	update(&db).unwrap();
	let results = browse(&db, Path::new("")).unwrap();

	assert_eq!(results.len(), 1);
	match results[0] {
		CollectionFile::Directory(ref d) => assert_eq!(d.path, root_path.to_str().unwrap()),
		_ => panic!("Expected directory"),
	}
}

#[test]
fn test_browse() {
	let mut khemmis_path = PathBuf::new();
	khemmis_path.push("root");
	khemmis_path.push("Khemmis");

	let mut tobokegao_path = PathBuf::new();
	tobokegao_path.push("root");
	tobokegao_path.push("Tobokegao");

	let db = db::_get_test_db("browse.sqlite");
	update(&db).unwrap();
	let results = browse(&db, Path::new("root")).unwrap();

	assert_eq!(results.len(), 2);
	match results[0] {
		CollectionFile::Directory(ref d) => assert_eq!(d.path, khemmis_path.to_str().unwrap()),
		_ => panic!("Expected directory"),
	}
	match results[1] {
		CollectionFile::Directory(ref d) => assert_eq!(d.path, tobokegao_path.to_str().unwrap()),
		_ => panic!("Expected directory"),
	}
}

#[test]
fn test_flatten() {
	let db = db::_get_test_db("flatten.sqlite");
	update(&db).unwrap();
	let results = flatten(&db, Path::new("root")).unwrap();
	assert_eq!(results.len(), 12);
	assert_eq!(results[0].title, Some("Above The Water".to_owned()));
}

#[test]
fn test_random() {
	let db = db::_get_test_db("random.sqlite");
	update(&db).unwrap();
	let results = get_random_albums(&db, 1).unwrap();
	assert_eq!(results.len(), 1);
}

#[test]
fn test_recent() {
	let db = db::_get_test_db("recent.sqlite");
	update(&db).unwrap();
	let results = get_recent_albums(&db, 2).unwrap();
	assert_eq!(results.len(), 2);
	assert!(results[0].date_added >= results[1].date_added);
}

#[test]
fn test_get_song() {
	let db = db::_get_test_db("get_song.sqlite");
	update(&db).unwrap();

	let mut song_path = PathBuf::new();
	song_path.push("root");
	song_path.push("Khemmis");
	song_path.push("Hunted");
	song_path.push("02 - Candlelight.mp3");

	let song = get_song(&db, &song_path).unwrap();
	assert_eq!(song.title.unwrap(), "Candlelight");
}
