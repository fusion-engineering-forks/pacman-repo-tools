use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use structopt::clap::AppSettings;
use structopt::StructOpt;

use pacman_repo_tools::db::{read_db_dir, DatabasePackage};
use pacman_repo_tools::msg::{use_color, Paint};
use pacman_repo_tools::parse::rpartition;
use pacman_repo_tools::{error, msg, plain, plain_no_eol, warning};

/// Download packages from a number of pacman repositories.
///
/// The order of repositories is significant in case multiple repositories have a package with the same name.
/// In that case, repositories mentioned earlier will be used.
/// Repositories mentioned with `--db-url` are always consulted before those read from a file.
#[derive(StructOpt)]
#[structopt(name = env!("CARGO_BIN_NAME"))]
#[structopt(setting = AppSettings::ColoredHelp)]
#[structopt(setting = AppSettings::UnifiedHelpMessage)]
#[structopt(setting = AppSettings::DeriveDisplayOrder)]
struct Options {
	/// Add a package to be downloaded.
	#[structopt(long, short)]
	#[structopt(value_name = "NAME")]
	pkg: Vec<String>,

	/// Read packages to download from a file, one package per line.
	#[structopt(long, short = "f")]
	#[structopt(value_name = "PATH")]
	pkg_file: Vec<PathBuf>,

	/// Download all packages.
	#[structopt(long, conflicts_with = "pkg", conflicts_with = "pkg_file")]
	pkg_all: bool,

	/// A repository to download packages from (specify the URL for the database archive).
	#[structopt(long)]
	#[structopt(value_name = "URL.db")]
	db_url: Vec<String>,

	/// Read repository database URLs from a file, one database URL per line.
	#[structopt(long, short)]
	#[structopt(value_name = "PATH")]
	db_file: Vec<PathBuf>,

	/// Save downloaded packages to this directory.
	#[structopt(long, short = "o")]
	#[structopt(value_name = "DIRECTORY")]
	#[structopt(default_value = "packages")]
	pkg_dir: PathBuf,

	/// Extract repository databases to this directory.
	#[structopt(long)]
	#[structopt(value_name = "DIRECTORY")]
	#[structopt(default_value = "db")]
	db_dir: PathBuf,

	/// Add the downloaded packages to a database.
	#[structopt(long)]
	#[structopt(value_name = "NAME")]
	add_to_db: Option<PathBuf>,

	/// Delete the database before adding packages to it.
	#[structopt(long)]
	#[structopt(requires = "add-to-db")]
	recreate_db: bool,

	/// Do not automatically download dependencies.
	#[structopt(long)]
	no_deps: bool,
}

fn main() {
	if !use_color() {
		Paint::disable();
	}

	let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
	let runtime = match runtime {
		Ok(x) => x,
		Err(e) => {
			error!("Failed to initialize tokio runtime: {}.", e);
			std::process::exit(1);
		},
	};
	runtime.block_on(async {
		if do_main(Options::from_args()).await.is_err() {
			std::process::exit(1);
		}
	})
}

async fn do_main(options: Options) -> Result<(), ()> {
	let targets = read_files_to_vec(options.pkg, &options.pkg_file)?;
	let databases = read_files_to_vec(options.db_url, &options.db_file)?;

	if targets.is_empty() && !options.pkg_all {
		error!("Need atleast one package to download.");
		return Err(());
	}

	if databases.is_empty() {
		error!("Need atleast one repository database.");
		return Err(());
	}

	let repositories = Repository::parse_urls(&databases)?;

	let http_client = reqwest::Client::new();

	msg!("Syncing repository databases");
	let packages = sync_dbs(&http_client, &options.db_dir, &repositories).await?;
	let packages = index_packages_by_name(&packages);

	let selected_packages = if options.pkg_all {
		packages.keys().copied().collect()
	} else if options.no_deps {
		targets.iter().map(String::as_str).collect()
	} else {
		let resolver = DependencyResolver::new(&packages);
		resolver.resolve(&targets)?
	};

	msg!("Downloading packages");
	let downloaded = download_packages(&http_client, &options.pkg_dir, &selected_packages, &packages).await?;

	if let Some(db_path) = options.add_to_db {
		msg!("Adding packages to {}", Paint::blue(db_path.display()).bold());
		if options.recreate_db {
			// If we create a fresh database, add all selected packages.
			remove_file(&db_path)?;
			let selected: Vec<_> = selected_packages.iter().map(|name| *packages.get(name).unwrap()).collect();
			add_to_database(&db_path, &options.pkg_dir, &selected).await?;
		} else {
			// Otherwise, only add downloaded packages.
			add_to_database(&db_path, &options.pkg_dir, &downloaded).await?;
		}
	}

	Ok(())
}

/// Read the lines of a list of files into a vector.
///
/// Leading and trailing whitespace of each line is trimmed.
/// Empty lines and lines that start with a '#' (after stripping) are skipped.
fn read_files_to_vec(initial: Vec<String>, paths: &[impl AsRef<Path>]) -> Result<Vec<String>, ()> {
	let mut result = initial;

	for path in paths {
		let path = path.as_ref();
		let buffer = std::fs::read(&path).map_err(|e| error!("Failed to read {}: {}.", path.display(), e))?;
		let buffer = String::from_utf8(buffer).map_err(|e| error!("Invalid UTF-8 in {}: {}.", path.display(), e))?;

		result.extend(buffer.lines().filter_map(|line| {
			let line = line.trim();
			if line.is_empty() || line.starts_with('#') {
				None
			} else {
				Some(String::from(line))
			}
		}));
	}

	Ok(result)
}

/// Metadata about a repository.
struct Repository {
	name: String,
	db_url: reqwest::Url,
}

impl Repository {
	/// Parse a list of repository URLs.
	///
	/// If different URLs refer to repositories with the same name,
	/// an error is returned.
	fn parse_urls(urls: &[impl AsRef<str>]) -> Result<Vec<Repository>, ()> {
		let mut names = BTreeSet::new();
		let mut repositories = Vec::with_capacity(urls.len());
		for url in urls {
			let repository: Repository = url.as_ref().parse()?;
			if !names.insert(repository.name.clone()) {
				error!("Duplicate repository name: {}.", repository.name);
				return Err(());
			}
			repositories.push(repository);
		}

		Ok(repositories)
	}
}

impl std::str::FromStr for Repository {
	type Err = ();

	fn from_str(input: &str) -> Result<Self, Self::Err> {
		let db_url: reqwest::Url = input.parse().map_err(|e| error!("Invalid URL: {}: {}.", input, e))?;
		let name = rpartition(db_url.path(), '/').map(|(_, name)| name).unwrap_or_else(|| db_url.path());
		if name.is_empty() {
			error!("Can not determine repository name from URL: {}.", input);
			return Err(());
		}
		Ok(Self { name: name.into(), db_url })
	}
}

/// Download and extract the given database files specified by the URLs to the given directory.
async fn sync_dbs<'a>(
	http_client: &reqwest::Client,
	directory: impl AsRef<Path>,
	repositories: &'a [Repository],
) -> Result<Vec<(&'a Repository, Vec<DatabasePackage>)>, ()> {
	let directory = directory.as_ref();

	let mut repo_packages = Vec::new();

	for (i, repo) in repositories.iter().enumerate() {
		let db_dir = directory.join(&repo.name);
		download_database(http_client, &db_dir, &repo.db_url, i, repositories.len()).await?;

		let packages = read_db_dir(&db_dir).map_err(|e| error!("{}.", e))?;
		repo_packages.push((repo, packages));
	}

	Ok(repo_packages)
}

/// Index packages from different repositories by name.
///
/// If multiple packages from  different repositories contain packages with the same name,
/// only the package from the first repository is used.
fn index_packages_by_name<'a>(packages: &'a [(&'a Repository, Vec<DatabasePackage>)]) -> BTreeMap<&'a str, (&'a Repository, &'a DatabasePackage)> {
	use std::collections::btree_map::Entry;

	let mut index: BTreeMap<&str, (&Repository, &DatabasePackage)> = BTreeMap::new();
	for (repo, packages) in packages {
		for package in packages {
			match index.entry(package.name.as_str()) {
				Entry::Occupied(x) => {
					let (prev_repo, _) = x.get();
					warning!(
						"Package {} already encountered in {}, ignoring package from {}.",
						package.name,
						prev_repo.name,
						repo.name
					);
				},
				Entry::Vacant(entry) => {
					entry.insert((repo, package));
				},
			}
		}
	}

	index
}

/// Create an index of virtual target names to concrete packages that provide the target.
fn index_providers<'a>(packages: &BTreeMap<&'a str, (&'a Repository, &'a DatabasePackage)>) -> BTreeMap<&'a str, BTreeSet<&'a str>> {
	let mut index: BTreeMap<&'a str, BTreeSet<&'a str>> = BTreeMap::new();
	for (_repo, package) in packages.values() {
		index.entry(&package.name).or_default().insert(&package.name);
		for target in &package.provides {
			index.entry(&target.name).or_default().insert(&package.name);
		}
	}
	index
}

/// Recursive dependency resolver.
struct DependencyResolver<'a, 'b> {
	packages: &'b BTreeMap<&'a str, (&'a Repository, &'a DatabasePackage)>,
	providers: BTreeMap<&'a str, BTreeSet<&'a str>>,
	selected_packages: BTreeSet<&'a str>,
	provided_targets: BTreeSet<&'a str>,
}

impl<'a, 'b> DependencyResolver<'a, 'b> {
	/// Create a new dependency resolver.
	pub fn new(packages: &'b BTreeMap<&'a str, (&'a Repository, &'a DatabasePackage)>) -> Self {
		Self {
			packages,
			providers: index_providers(&packages),
			selected_packages: BTreeSet::new(),
			provided_targets: BTreeSet::new(),
		}
	}

	/// Resolve the targets into a set of packages to download.
	///
	/// This will recursively resolve all dependencies and virtual targets.
	///
	/// Dependencies and virtual targets that are already provided by a selected package are skipped.
	/// Howwever, all real packages given in `targets` will be selected.
	pub fn resolve(mut self, targets: &[impl AsRef<str>]) -> Result<BTreeSet<&'a str>, ()> {
		let mut queue = BTreeSet::new();

		for target in targets {
			let target = target.as_ref();
			// First add all explicitly listed real packages.
			if let Some((_repo, package)) = self.packages.get(target) {
				self.add_package(package);
				for depend in &package.depends {
					queue.insert(depend.name.as_str());
				}
			// Add virtual targets to the queue to be resolved later.
			// They may already be provided by an explicitly listed package.
			} else {
				queue.insert(target);
			}
		}

		// Resolve targets in the queue until it is empty.
		while let Some(target) = pop_first(&mut queue) {
			// Ignore already-provided targets.
			// All explicitly listed packages have already been added,
			// so these are either virtual targets or dependencies.
			if self.provided_targets.contains(target) {
				continue;
			}

			let package = self.resolve_target(target)?;
			self.add_package(package);
			for depend in &package.depends {
				if !self.provided_targets.contains(depend.name.as_str()) {
					queue.insert(&depend.name);
				}
			}
		}

		Ok(self.selected_packages)
	}

	/// Add a package to the selection.
	fn add_package(&mut self, package: &'a DatabasePackage) {
		self.selected_packages.insert(&package.name);
		self.provided_targets.insert(&package.name);
		let provides = package.provides.iter().map(|x| x.name.as_str());
		self.provided_targets.extend(provides);
	}

	/// Choose a package for a target.
	///
	/// If the target is a concrete package, choose that.
	/// Otherwise, choose some implementation defined provider, if it exists.
	fn resolve_target(&self, target: &str) -> Result<&'a DatabasePackage, ()> {
		if let Some((_repo, package)) = self.packages.get(target) {
			Ok(package)
		} else {
			let provider = self
				.providers
				.get(target)
				.and_then(|x| x.iter().next())
				.ok_or_else(|| error!("No provider found for target: {}.", target))?;
			self.packages
				.get(provider)
				.map(|&(_repo, package)| package)
				.ok_or_else(|| error!("No such package: {}.", provider))
		}
	}
}

/// Pop the first entry from a BTreeSet.
fn pop_first<T: Copy + Ord>(set: &mut BTreeSet<T>) -> Option<T> {
	let value = *set.iter().next()?;
	set.take(&value)
}

/// Download and extract a database file.
async fn download_database(http_client: &reqwest::Client, directory: &Path, url: &reqwest::Url, index: usize, total: usize) -> Result<(), ()> {
	plain_no_eol!(
		"Downloading [{}/{}] {}...",
		Paint::blue(index + 1).bold(),
		Paint::blue(total).bold(),
		Paint::cyan(url)
	);
	let last_modified_path = directory.join("last-modified");
	let etag_path = directory.join("etag");
	let last_modified = std::fs::read_to_string(&last_modified_path).ok();
	let etag = std::fs::read_to_string(&etag_path).ok();

	let download = maybe_download(http_client, &url, last_modified.as_deref(), etag.as_deref())
		.await
		.map_err(|e| {
			println!(" {}", Paint::red("failed"));
			error!("{}.", e);
		})?;

	if let Some(download) = download {
		println!(" {}", Paint::green("done"));
		let _: Result<_, _> = std::fs::remove_file(&last_modified_path);
		let _: Result<_, _> = std::fs::remove_file(&etag_path);
		extract_archive(&directory, &download.data).await?;
		if let Some(last_modified) = download.last_modified {
			let _: Result<_, _> = std::fs::write(&last_modified_path, last_modified);
		}
		if let Some(etag) = download.etag {
			let _: Result<_, _> = std::fs::write(&etag_path, etag);
		}
	} else {
		println!(" {}", Paint::yellow("up to date"));
	}
	Ok(())
}

/// Download all packages.
async fn download_packages<'a>(
	http_client: &reqwest::Client,
	directory: &impl AsRef<Path>,
	selected: &BTreeSet<&str>,
	packages: &BTreeMap<&str, (&'a Repository, &'a DatabasePackage)>,
) -> Result<Vec<(&'a Repository, &'a DatabasePackage)>, ()> {
	let directory = directory.as_ref();
	let mut downloaded = Vec::with_capacity(selected.len());
	for (i, pkg_name) in selected.iter().enumerate() {
		let (repository, package) = packages
			.get(pkg_name)
			.unwrap_or_else(|| panic!("selected package list contains unknown package: {}", pkg_name));
		if download_package(http_client, directory, repository, package, i, selected.len()).await? {
			downloaded.push((*repository, *package));
		}
	}
	Ok(downloaded)
}

/// Download a single package, if required.
async fn download_package(
	http_client: &reqwest::Client,
	directory: impl AsRef<Path>,
	repository: &Repository,
	package: &DatabasePackage,
	index: usize,
	total: usize,
) -> Result<bool, ()> {
	use std::io::Write;
	let directory = directory.as_ref();
	make_dirs(directory)?;

	let pkg_url = package_url(repository, package);
	let pkg_path = directory.join(&package.filename);
	let skip = if let Some(metadata) = stat(&pkg_path)? {
		if metadata.len() != package.compressed_size {
			warning!("File size of {} does not match, re-downloading package.", package.filename);
			false
		} else if !file_sha256(&pkg_path)?.eq_ignore_ascii_case(&package.sha256sum) {
			warning!("SHA256 checksum of {} does not match, re-downloading package.", package.filename);
			false
		} else {
			true
		}
	} else {
		false
	};

	plain_no_eol!(
		"Downloading [{}/{}] {}...",
		Paint::blue(index + 1).bold(),
		Paint::blue(total).bold(),
		Paint::cyan(&package.name)
	);
	if skip {
		println!(" {}", Paint::yellow("up to date"));
		return Ok(false);
	}
	let mut file = std::fs::File::create(&pkg_path).map_err(|e| {
		println!(" {}", Paint::red("failed"));
		error!("Failed to open {} for writing: {}.", pkg_path.display(), e);
	})?;
	let data = download(http_client, &pkg_url).await.map_err(|e| {
		println!(" {}", Paint::red("failed"));
		error!("{}.", e);
	})?;
	file.write_all(&data).map_err(|e| {
		println!(" {}", Paint::red("failed"));
		error!("Failed to write to {}: {}.", pkg_path.display(), e);
	})?;
	println!(" {}", Paint::green("done"));
	Ok(true)
}

/// Get the URL of a package file.
fn package_url(repository: &Repository, package: &DatabasePackage) -> reqwest::Url {
	let db_path = repository.db_url.path();
	let parent = rpartition(db_path, '/').map(|(parent, _db_name)| parent).unwrap_or("");

	let mut pkg_url = repository.db_url.clone();
	pkg_url.set_path(&format!("{}/{}", parent, package.filename));
	pkg_url
}

/// Add packages to a database.
async fn add_to_database(db_path: &Path, pkg_dir: &Path, packages: &[(&Repository, &DatabasePackage)]) -> Result<(), ()> {
	if packages.is_empty() {
		plain!("No packages to add.");
		return Ok(());
	}

	if let Some(parent) = db_path.parent() {
		make_dirs(parent)?;
	}

	for (i, (_repo, package)) in packages.iter().enumerate() {
		if Paint::is_enabled() && i != 0 {
			print!("\x1b[F"); // Go up one line.
			plain_no_eol!("Processing package {}/{}.", i + 1, packages.len());
			print!("\x1b[K"); // Clear to end of line.
			println!();
		} else {
			plain!("Processing package {}/{}.", i + 1, packages.len());
		}

		let status = tokio::process::Command::new("repo-add")
			.arg("-q")
			.arg(&db_path)
			.arg(pkg_dir.join(&package.filename))
			.stdin(std::process::Stdio::null())
			.spawn()
			.map_err(|e| error!("Failed to run repo-add: {}", e))?
			.wait()
			.await
			.map_err(|e| error!("Failed to wait for repo-add to finish: {}.", e))?;
		if !status.success() {
			error!("repo-add exited with {}.", status);
			return Err(());
		}
	}

	Ok(())
}

struct Download {
	data: Vec<u8>,
	last_modified: Option<String>,
	etag: Option<String>,
}

/// Download a file over HTTP(S).
async fn download(client: &reqwest::Client, url: &reqwest::Url) -> Result<Vec<u8>, reqwest::Error> {
	let response = client.get(url.clone()).send().await?.error_for_status()?;
	Ok(response.bytes().await?.to_vec())
}

/// Download a file over HTTP(S).
async fn maybe_download(
	client: &reqwest::Client,
	url: &reqwest::Url,
	last_modified: Option<&str>,
	etag: Option<&str>,
) -> Result<Option<Download>, reqwest::Error> {
	let mut request = client.get(url.clone());
	if let Some(last_modified) = last_modified {
		request = request.header("If-Modified-Since", last_modified);
	}
	if let Some(etag) = etag {
		request = request.header("If-None-Match", etag);
	}

	let response = request.send().await?.error_for_status()?;
	if response.status() == reqwest::StatusCode::NOT_MODIFIED {
		return Ok(None);
	}

	let last_modified = get_string_header(response.headers(), "Last-Modified");
	let etag = get_string_header(response.headers(), "ETag");
	let data = response.bytes().await?.to_vec();
	Ok(Some(Download { data, last_modified, etag }))
}

/// Get the value of a header as string.
///
/// If the header is not present or not a valid string, this returns `None`.
fn get_string_header(headers: &reqwest::header::HeaderMap, name: impl reqwest::header::AsHeaderName) -> Option<String> {
	Some(headers.get(name)?.to_str().ok()?.to_owned())
}

/// Create a directory and all parent directories as needed.
fn make_dirs(path: impl AsRef<Path>) -> Result<(), ()> {
	let path = path.as_ref();
	std::fs::create_dir_all(path).map_err(|e| error!("Failed to create directory {}: {}.", path.display(), e))
}

/// Recursively remove a directory and it's content.
fn remove_dir_all(path: impl AsRef<Path>) -> Result<(), ()> {
	let path = path.as_ref();
	match std::fs::remove_dir_all(path) {
		Ok(()) => Ok(()),
		Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
		Err(e) => {
			error!("Failed to remove directory {} or it's content: {}.", path.display(), e);
			Err(())
		},
	}
}

/// Get metadata for a file path.
///
/// Returns Ok(None) if the path does not exist.
fn stat(path: impl AsRef<Path>) -> Result<Option<std::fs::Metadata>, ()> {
	let path = path.as_ref();
	match path.metadata() {
		Ok(x) => Ok(Some(x)),
		Err(e) => {
			if e.kind() == std::io::ErrorKind::NotFound {
				Ok(None)
			} else {
				error!("Failed to stat {}: {}.", path.display(), e);
				Err(())
			}
		},
	}
}

/// Compute the sha256 checksum of the contents of a file.
///
/// Returns the sha256 digest as hex string.
fn file_sha256(path: impl AsRef<Path>) -> Result<String, ()> {
	use sha2::Digest;
	let path = path.as_ref();
	let data = std::fs::read(path).map_err(|e| error!("Failed to read {}: {}.", path.display(), e))?;
	let digest = sha2::Sha256::digest(&data);
	let mut hex = String::with_capacity(256 / 8 * 2);
	for byte in digest {
		hex += &format!("{:02x}", byte);
	}
	Ok(hex)
}

/// Extract an archive in a directory using bsdtar.
async fn extract_archive(directory: &Path, data: &[u8]) -> Result<(), ()> {
	use tokio::io::AsyncWriteExt;

	// Delete and re-create directory for extracting the archive.
	remove_dir_all(directory)?;
	make_dirs(directory)?;

	// Spawn bsdtar process.
	let mut process = tokio::process::Command::new("bsdtar")
		.args(&["xf", "-"])
		.current_dir(directory)
		.stdin(std::process::Stdio::piped())
		.spawn()
		.map_err(|e| error!("Failed to run bsdtar: {}.", e))?;

	// Write archive to standard input of bsdtar.
	let mut stdin = process.stdin.take().ok_or_else(|| error!("Failed to get stdin for bsdtar."))?;
	stdin
		.write_all(data)
		.await
		.map_err(|e| error!("Failed to write to bsdtar stdin: {}.", e))?;
	drop(stdin);

	// Wait for bsdtar to finish.
	let exit_status = process.wait().await.map_err(|e| error!("Failed to wait for bsdtar to exit: {}.", e))?;

	// Check the exit status.
	if exit_status.success() {
		Ok(())
	} else {
		error!("bsdtar exitted with {}.", exit_status);
		Err(())
	}
}

/// Remove a file.
///
/// Unlike [`std::fs::remove_file`], this function does not return an error if the file does not exist.
fn remove_file(path: impl AsRef<Path>) -> Result<(), ()> {
	let path = path.as_ref();
	if let Err(e) = std::fs::remove_file(path) {
		if e.kind() != std::io::ErrorKind::NotFound {
			error!("Failed to delete {}: {}.", path.display(), e);
			return Err(());
		}
	}
	Ok(())
}
