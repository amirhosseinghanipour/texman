use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use chrono::{DateTime, Utc, Duration};
use std::fs;
use futures::future::join_all;
use futures::StreamExt;
use xz2::read::XzDecoder;
use tar;
use rusqlite::{Connection, params, OptionalExtension};
use indicatif::{ProgressBar, ProgressStyle};
use std::io::Write;
use rayon::prelude::*;

#[derive(Parser)]
#[command(name = "texman", about = "A Rust-based LaTeX package manager", version = "0.1.0")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Install {
        package: String,
        #[arg(long, default_value = "default")]
        profile: String,
    },
    Update,
    List,
    Remove {
        package: String,
    },
    Info {
        package: String,
    },
    Backup {
        #[command(subcommand)]
        action: BackupAction,
    },
    Restore {
        name: String,
    },
    Search {
        term: String,
        #[arg(long)]
        description: bool,
        #[arg(long)]
        depends: bool,
        #[arg(long)]
        longdesc: bool,
    },
    Clean {
        #[arg(long)]
        backups: bool,
    },
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
}

#[derive(Subcommand)]
enum ProfileAction {
    Create { name: String },
    Switch { name: String },
    List,
    Remove { name: String },
}

#[derive(Subcommand)]
enum BackupAction {
    Create { name: String },
    List,
    Remove { name: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Package {
    name: String,
    revision: String,
    url: String,
    depends: Vec<String>,
    runfiles: Vec<String>,
    binfiles: Vec<String>,
    description: Option<String>,
    longdesc: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    let tlpdb = fetch_tlpdb().await?;

    match cli.command {
        Commands::Install { package, profile } => {
            log::info!("Installing package: {} into profile: {}", package, profile);
            install_package(&package, &profile, &tlpdb).await?;
        }
        Commands::Update => {
            log::info!("Updating packages in active profile");
            update_packages(&tlpdb).await?;
        }
        Commands::List => {
            log::info!("Listing installed packages in active profile");
            list_packages()?;
        }
        Commands::Remove { package } => {
            log::info!("Removing package: {}", package);
            remove_package(&package)?;
        }
        Commands::Info { package } => {
            log::info!("Showing info for package: {}", package);
            info_package(&package, &tlpdb)?;
        }
        Commands::Backup { action } => match action {
            BackupAction::Create { name } => {
                log::info!("Backing up active profile to '{}'", name);
                backup_profile(&name)?;
            }
            BackupAction::List => {
                log::info!("Listing all backups");
                list_backups()?;
            }
            BackupAction::Remove { name } => {
                log::info!("Removing backup '{}'", name);
                remove_backup(&name)?;
            }
        },
        Commands::Restore { name } => {
            log::info!("Restoring active profile from backup '{}'", name);
            restore_profile(&name)?;
        }
        Commands::Search { term, description, depends, longdesc } => {
            log::info!("Searching for packages matching '{}'", term);
            search_packages(&term, &tlpdb, description, depends, longdesc)?;
        }
        Commands::Clean { backups } => {
            log::info!("Cleaning up unused files{}", if backups { " and backups" } else { "" });
            clean(backups)?;
        }
        Commands::Profile { action } => match action {
            ProfileAction::Create { name } => create_profile(&name)?,
            ProfileAction::Switch { name } => switch_profile(&name)?,
            ProfileAction::List => {
                log::info!("Listing all profiles");
                list_profiles()?;
            }
            ProfileAction::Remove { name } => {
                log::info!("Removing profile '{}'", name);
                remove_profile(&name)?;
            }
        },
    }

    Ok(())
}

fn init_db(texman_dir: &PathBuf) -> anyhow::Result<Connection> {
    let db_path = texman_dir.join("db").join("texman.sqlite");
    let conn = Connection::open(db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS installed_packages (
            profile TEXT NOT NULL,
            name TEXT NOT NULL,
            revision TEXT NOT NULL,
            PRIMARY KEY (profile, name)
        )",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS backups (
            backup_name TEXT NOT NULL,
            profile TEXT NOT NULL,
            name TEXT NOT NULL,
            revision TEXT NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            PRIMARY KEY (backup_name, name)
        )",
        [],
    )?;
    Ok(conn)
}

async fn fetch_tlpdb() -> anyhow::Result<HashMap<String, Package>> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let db_dir = texman_dir.join("db");
    let tlpdb_path = db_dir.join("tlpdb.txt");
    let tlpdb_bin_path = db_dir.join("tlpdb.bin");

    std::fs::create_dir_all(&db_dir)?;

    let should_fetch = if tlpdb_path.exists() {
        let metadata = fs::metadata(&tlpdb_path)?;
        let modified = metadata.modified()?;
        let last_modified: DateTime<Utc> = modified.into();
        let now = Utc::now();
        let age = now - last_modified;
        age > Duration::hours(24)
    } else {
        true
    };

    if !should_fetch && tlpdb_bin_path.exists() {
        let bin_file = File::open(&tlpdb_bin_path)?;
        let tlpdb: HashMap<String, Package> = bincode::deserialize_from(bin_file)
            .map_err(|e| anyhow::anyhow!("Failed to deserialize TLPDB: {}", e))?;
        log::info!("Loaded cached TLPDB from {:?}", tlpdb_bin_path);
        return Ok(tlpdb);
    }

    let tlpdb_text = if should_fetch {
        log::info!("Fetching fresh TLPDB from CTAN mirror");
        let text = fetch_tlpdb_text().await?;
        fs::write(&tlpdb_path, &text)?;
        log::info!("Cached TLPDB at {:?}", tlpdb_path);
        text
    } else {
        log::info!("Using cached TLPDB from {:?}", tlpdb_path);
        fs::read_to_string(&tlpdb_path)?
    };

    let tlpdb = parse_tlpdb(&tlpdb_text)?;
    let bin_file = File::create(&tlpdb_bin_path)?;
    bincode::serialize_into(bin_file, &tlpdb)
        .map_err(|e| anyhow::anyhow!("Failed to serialize TLPDB: {}", e))?;
    log::info!("Saved serialized TLPDB to {:?}", tlpdb_bin_path);

    Ok(tlpdb)
}

async fn fetch_tlpdb_text() -> anyhow::Result<String> {
    let url = "http://mirror.ctan.org/systems/texlive/tlnet/tlpkg/texlive.tlpdb";
    let response = reqwest::get(url).await?;
    let content_length = response.content_length().unwrap_or(0);
    let pb = ProgressBar::new(content_length);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {bytes}/{total_bytes} ({bytes_per_sec}, {eta}")?
            .progress_chars("##-")
    );

    let mut buffer = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.extend_from_slice(&chunk);
        pb.inc(chunk.len() as u64);
    }
    pb.finish_with_message("Downloaded TLPDB");

    let tlpdb_text = String::from_utf8(buffer)
        .map_err(|e| anyhow::anyhow!("Invalid UTF-8 in TLPDB: {}", e))?;
    log::debug!("Fetched TLPDB ({} bytes)", tlpdb_text.len());
    Ok(tlpdb_text)
}

fn parse_tlpdb(tlpdb_text: &str) -> anyhow::Result<HashMap<String, Package>> {
    let blocks: Vec<&str> = tlpdb_text.split("\n\n").filter(|b| !b.trim().is_empty()).collect();
    let packages: Vec<Package> = blocks.par_iter().filter_map(|block| {
        let mut pkg = Package {
            name: String::new(),
            revision: "unknown".to_string(),
            url: String::new(),
            depends: Vec::new(),
            runfiles: Vec::new(),
            binfiles: Vec::new(),
            description: None,
            longdesc: None,
        };
        let mut in_runfiles = false;
        let mut in_binfiles = false;
        let mut in_longdesc = false;
        let mut longdesc_lines = Vec::new();

        for line in block.lines() {
            let line = line.trim();
            if in_longdesc {
                if line.is_empty() || line.starts_with("name ") {
                    in_longdesc = false;
                    pkg.longdesc = Some(longdesc_lines.join("\n"));
                    longdesc_lines.clear();
                } else {
                    longdesc_lines.push(line.to_string());
                    continue;
                }
            }

            if line.starts_with("name ") {
                pkg.name = line[5..].to_string();
                pkg.url = format!("http://mirror.ctan.org/systems/texlive/tlnet/archive/{}.tar.xz", pkg.name);
            } else if line == "runfiles" {
                in_runfiles = true;
                in_binfiles = false;
            } else if line == "binfiles" {
                in_runfiles = false;
                in_binfiles = true;
            } else if line.starts_with("depends ") {
                let deps = &line[8..];
                if !deps.is_empty() {
                    pkg.depends.extend(deps.split(',').map(|s| s.trim().to_string()));
                }
                in_runfiles = false;
                in_binfiles = false;
            } else if line.starts_with("revision ") {
                pkg.revision = line[9..].to_string();
                in_runfiles = false;
                in_binfiles = false;
            } else if line.starts_with("shortdesc ") {
                pkg.description = Some(line[10..].to_string());
                in_runfiles = false;
                in_binfiles = false;
            } else if line.starts_with("longdesc ") {
                in_longdesc = true;
                longdesc_lines.push(line[9..].to_string());
                in_runfiles = false;
                in_binfiles = false;
            } else if in_runfiles && line.starts_with(' ') {
                pkg.runfiles.push(line.trim_start().to_string());
            } else if in_binfiles && line.starts_with(' ') {
                pkg.binfiles.push(line.trim_start().to_string());
            }
        }

        if in_longdesc && !longdesc_lines.is_empty() {
            pkg.longdesc = Some(longdesc_lines.join("\n"));
        }

        if pkg.name.is_empty() { None } else { Some(pkg) }
    }).collect();

    let mut tlpdb = HashMap::with_capacity(packages.len());
    for pkg in packages {
        tlpdb.insert(pkg.name.clone(), pkg);
    }

    log::info!("Parsed {} packages from TLPDB", tlpdb.len());
    Ok(tlpdb)
}

fn resolve_dependencies(
    package: &str,
    tlpdb: &HashMap<String, Package>,
    resolved: &mut Vec<String>,
    visited: &mut Vec<String>,
) -> anyhow::Result<()> {
    let pkg = tlpdb.get(package).ok_or_else(|| anyhow::anyhow!("Package '{}' not found in TLPDB", package))?;

    if visited.contains(&pkg.name) && !resolved.contains(&pkg.name) {
        anyhow::bail!("Circular dependency detected involving '{}'", pkg.name);
    }

    visited.push(pkg.name.clone());

    for dep in &pkg.depends {
        if !resolved.contains(dep) {
            log::debug!("Resolving dependency: {}", dep);
            resolve_dependencies(dep, tlpdb, resolved, visited)?;
            resolved.push(dep.clone());
        }
    }

    if !resolved.contains(&pkg.name) {
        resolved.push(pkg.name.clone());
    }

    Ok(())
}

async fn download_package(pkg: &Package, texman_dir: &PathBuf) -> anyhow::Result<PathBuf> {
    let platform = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let platform_suffix = match (platform, os) {
        ("x86_64", "linux") => "x86_64-linux",
        ("x86_64", "macos") => "x86_64-darwin",
        _ => "",
    };

    let mut archive_name = format!("{}.tar.xz", pkg.name);
    let mut url = pkg.url.clone();

    for file in &pkg.binfiles {
        if file.ends_with(&format!("{}.{}.tar.xz", pkg.name, platform_suffix)) {
            archive_name = format!("{}.{}.tar.xz", pkg.name, platform_suffix);
            url = format!(
                "http://mirror.ctan.org/systems/texlive/tlnet/archive/{}",
                archive_name
            );
            break;
        }
    }

    if url == pkg.url {
        for file in &pkg.runfiles {
            if file.ends_with(&format!("{}.tar.xz", pkg.name)) {
                archive_name = format!("{}.tar.xz", pkg.name);
                url = format!(
                    "http://mirror.ctan.org/systems/texlive/tlnet/archive/{}",
                    archive_name
                );
                break;
            }
        }
    }

    let download_path = texman_dir.join(&archive_name);
    log::info!("Downloading {} r{} from {}", pkg.name, pkg.revision, url);
    let response = reqwest::get(&url).await
        .map_err(|e| anyhow::anyhow!("Failed to download {}: {}", url, e))?;
    let content_length = response.content_length().unwrap_or(0);
    let pb = ProgressBar::new(content_length);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.green/yellow} {bytes}/{total_bytes} ({bytes_per_sec}, {eta}")?
            .progress_chars("##-")
    );

    let mut file = File::create(&download_path)?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk)?;
        pb.inc(chunk.len() as u64);
    }
    pb.finish_with_message(format!("Downloaded {}", pkg.name));

    Ok(download_path)
}

async fn install_package(package: &str, profile: &str, tlpdb: &HashMap<String, Package>) -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let profile_dir = texman_dir.join("profiles").join(profile);
    std::fs::create_dir_all(&profile_dir)?;

    let conn = init_db(&texman_dir)?;

    let mut to_install = Vec::new();
    let mut visited = Vec::new();
    resolve_dependencies(package, tlpdb, &mut to_install, &mut visited)?;

    if to_install.is_empty() {
        log::info!("No packages to install ({} already resolved)", package);
        return Ok(());
    }
    log::info!("Packages to install: {:?}", to_install);

    let packages: Vec<Package> = to_install
        .iter()
        .map(|pkg_name| tlpdb.get(pkg_name).unwrap().clone())
        .collect();

    let download_tasks: Vec<_> = packages
        .iter()
        .map(|pkg| {
            let pkg = pkg.clone();
            let texman_dir = texman_dir.clone();
            tokio::spawn(async move { download_package(&pkg, &texman_dir).await })
        })
        .collect();

    let download_results = join_all(download_tasks).await;
    let download_paths: Vec<PathBuf> = download_results
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("Task failed: {}", e))?
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("Download failed: {}", e))?;

    for (pkg, download_path) in packages.iter().zip(download_paths.iter()) {
        let store_path = profile_dir.join(format!("{}-r{}", pkg.name, pkg.revision));
        std::fs::create_dir_all(&store_path)?;

        log::info!("Installing {} r{} to {:?}", pkg.name, pkg.revision, store_path);
        let tar_xz = File::open(download_path)?;
        let tar = XzDecoder::new(tar_xz);
        let mut archive = tar::Archive::new(tar);
        archive.unpack(&store_path)
            .map_err(|e| anyhow::anyhow!("Failed to unpack {}: {}", pkg.name, e))?;

        std::fs::remove_file(download_path)?;

        conn.execute(
            "INSERT OR REPLACE INTO installed_packages (profile, name, revision) VALUES (?1, ?2, ?3)",
            params![profile, pkg.name, pkg.revision],
        )?;
        log::info!("Installed {} r{}", pkg.name, pkg.revision);
    }

    let active_path = texman_dir.join("active");
    if !active_path.exists() {
        std::os::unix::fs::symlink(&profile_dir, &active_path)?;
        log::info!("Set {} as active profile", profile);
    }

    Ok(())
}

async fn update_packages(tlpdb: &HashMap<String, Package>) -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let active_path = texman_dir.join("active");

    if !active_path.exists() {
        anyhow::bail!("No active profile set. Install a package or switch to a profile first.");
    }

    let conn = init_db(&texman_dir)?;
    let active_dir = fs::canonicalize(&active_path)?;
    let active_profile = active_path.read_link()?
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let mut to_update = Vec::new();
    let mut stmt = conn.prepare("SELECT name, revision FROM installed_packages WHERE profile = ?1")?;
    let rows = stmt.query_map(params![active_profile], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    for row in rows {
        let (pkg_name, current_revision) = row?;
        if let Some(latest_pkg) = tlpdb.get(&pkg_name) {
            let current_rev: u32 = current_revision.parse()
                .map_err(|e| anyhow::anyhow!("Invalid revision {} for {}: {}", current_revision, pkg_name, e))?;
            let latest_rev: u32 = latest_pkg.revision.parse()
                .map_err(|e| anyhow::anyhow!("Invalid revision {} for {}: {}", latest_pkg.revision, pkg_name, e))?;
            if latest_rev > current_rev {
                log::info!("Found update for {}: r{} -> r{}", pkg_name, current_revision, latest_pkg.revision);
                to_update.push(latest_pkg.clone());
            }
        }
    }

    if to_update.is_empty() {
        log::info!("All packages are up to date");
        return Ok(());
    }

    let download_tasks: Vec<_> = to_update
        .iter()
        .map(|pkg| {
            let pkg = pkg.clone();
            let texman_dir = texman_dir.clone();
            tokio::spawn(async move { download_package(&pkg, &texman_dir).await })
        })
        .collect();

    let download_results = join_all(download_tasks).await;
    let download_paths: Vec<PathBuf> = download_results
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("Task failed during update: {}", e))?
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("Download failed during update: {}", e))?;

    for (pkg, download_path) in to_update.iter().zip(download_paths.iter()) {
        let store_path = active_dir.join(format!("{}-r{}", pkg.name, pkg.revision));
        std::fs::create_dir_all(&store_path)?;

        log::info!("Updating {} r{} to {:?}", pkg.name, pkg.revision, store_path);
        let tar_xz = File::open(download_path)?;
        let tar = XzDecoder::new(tar_xz);
        let mut archive = tar::Archive::new(tar);
        archive.unpack(&store_path)
            .map_err(|e| anyhow::anyhow!("Failed to unpack {}: {}", pkg.name, e))?;

        std::fs::remove_file(download_path)?;

        conn.execute(
            "INSERT OR REPLACE INTO installed_packages (profile, name, revision) VALUES (?1, ?2, ?3)",
            params![active_profile, pkg.name, pkg.revision],
        )?;
        log::info!("Updated {} r{}", pkg.name, pkg.revision);

        let old_path = active_dir.join(format!("{}-r{}", pkg.name, pkg.revision));
        if old_path.exists() && old_path != store_path {
            fs::remove_dir_all(&old_path)?;
            log::info!("Removed old version of {}", pkg.name);
        }
    }

    Ok(())
}

fn list_packages() -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let active_path = texman_dir.join("active");

    if !active_path.exists() {
        anyhow::bail!("No active profile set. Install a package or switch to a profile first.");
    }

    let conn = init_db(&texman_dir)?;
    let active_profile = active_path.read_link()?
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let mut stmt = conn.prepare("SELECT name, revision FROM installed_packages WHERE profile = ?1 ORDER BY name")?;
    let rows = stmt.query_map(params![active_profile], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    println!("Installed packages in profile '{}':", active_profile);
    for row in rows {
        let (name, revision) = row?;
        println!("  {} r{}", name, revision);
    }

    Ok(())
}

fn remove_package(package: &str) -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let active_path = texman_dir.join("active");

    if !active_path.exists() {
        anyhow::bail!("No active profile set. Install a package or switch to a profile first.");
    }

    let conn = init_db(&texman_dir)?;
    let active_dir = fs::canonicalize(&active_path)?;
    let active_profile = active_path.read_link()?
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let mut stmt = conn.prepare("SELECT revision FROM installed_packages WHERE profile = ?1 AND name = ?2")?;
    let revision: Option<String> = stmt.query_row(params![active_profile, package], |row| row.get(0)).optional()?;

    if let Some(revision) = revision {
        let store_path = active_dir.join(format!("{}-r{}", package, revision));
        if store_path.exists() {
            fs::remove_dir_all(&store_path)?;
            log::info!("Removed files for {} r{}", package, revision);
        }

        conn.execute(
            "DELETE FROM installed_packages WHERE profile = ?1 AND name = ?2",
            params![active_profile, package],
        )?;
        log::info!("Removed {} from profile '{}'", package, active_profile);
    } else {
        log::warn!("Package {} not found in profile '{}'", package, active_profile);
    }

    Ok(())
}

fn info_package(package: &str, tlpdb: &HashMap<String, Package>) -> anyhow::Result<()> {
    let pkg = tlpdb.get(package).ok_or_else(|| anyhow::anyhow!("Package '{}' not found in TLPDB", package))?;
    
    println!("Package: {}", pkg.name);
    println!("Revision: {}", pkg.revision);
    println!("Default URL: {}", pkg.url);
    let deps_str = if pkg.depends.is_empty() { "None".to_string() } else { pkg.depends.join(", ") };
    println!("Dependencies: {}", deps_str);
    if let Some(desc) = &pkg.description {
        println!("Short Description: {}", desc);
    }
    if let Some(longdesc) = &pkg.longdesc {
        println!("Long Description: {}", longdesc);
    }
    println!("Runfiles ({}):", pkg.runfiles.len());
    for file in &pkg.runfiles {
        println!("  {}", file);
    }
    println!("Binfiles ({}):", pkg.binfiles.len());
    for file in &pkg.binfiles {
        println!("  {}", file);
    }

    Ok(())
}

fn search_packages(term: &str, tlpdb: &HashMap<String, Package>, search_desc: bool, search_deps: bool, search_longdesc: bool) -> anyhow::Result<()> {
    let term_lower = term.to_lowercase();
    let mut matches: Vec<&Package> = tlpdb
        .values()
        .filter(|pkg| {
            let name_match = pkg.name.to_lowercase().contains(&term_lower);
            let desc_match = search_desc && pkg.description.as_ref().map_or(false, |d| d.to_lowercase().contains(&term_lower));
            let longdesc_match = search_longdesc && pkg.longdesc.as_ref().map_or(false, |d| d.to_lowercase().contains(&term_lower));
            let deps_match = search_deps && pkg.depends.iter().any(|d| d.to_lowercase().contains(&term_lower));
            name_match || desc_match || longdesc_match || deps_match
        })
        .collect();
    
    if matches.is_empty() {
        println!("No packages found matching '{}'", term);
        return Ok(());
    }

    matches.sort_by(|a, b| a.name.cmp(&b.name));
    println!("Found {} packages matching '{}':", matches.len(), term);
    for pkg in matches {
        println!("  {} r{}", pkg.name, pkg.revision);
        if search_desc && pkg.description.is_some() {
            println!("    Short Description: {}", pkg.description.as_ref().unwrap());
        }
        if search_longdesc && pkg.longdesc.is_some() {
            println!("    Long Description: {}", pkg.longdesc.as_ref().unwrap());
        }
        if search_deps && !pkg.depends.is_empty() {
            println!("    Depends: {}", pkg.depends.join(", "));
        }
    }

    Ok(())
}

fn create_profile(name: &str) -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let profile_path = texman_dir.join("profiles").join(name);
    std::fs::create_dir_all(&profile_path)?;
    log::info!("Created profile: {}", name);
    Ok(())
}

fn switch_profile(name: &str) -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let profile_path = texman_dir.join("profiles").join(name);
    let active_path = texman_dir.join("active");

    if !profile_path.exists() {
        anyhow::bail!("Profile '{}' does not exist. Use 'profile create {}' to create it.", name, name);
    }

    if active_path.exists() {
        std::fs::remove_file(&active_path)?;
    }
    std::os::unix::fs::symlink(&profile_path, &active_path)?;
    log::info!("Switched to profile: {}", name);
    Ok(())
}

fn list_profiles() -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let profiles_dir = texman_dir.join("profiles");
    let active_path = texman_dir.join("active");

    if !profiles_dir.exists() {
        println!("No profiles found.");
        return Ok(());
    }

    let mut profiles = Vec::new();
    for entry in fs::read_dir(&profiles_dir)? {
        let entry = entry?;
        let name = entry.file_name().into_string().unwrap();
        profiles.push(name);
    }

    if profiles.is_empty() {
        println!("No profiles found.");
        return Ok(());
    }

    let active_profile = if active_path.exists() {
        active_path.read_link()?
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    } else {
        String::new()
    };

    println!("Available profiles:");
    for profile in profiles {
        let active_mark = if profile == active_profile { " (active)" } else { "" };
        println!("  {}{}", profile, active_mark);
    }

    Ok(())
}

fn remove_profile(name: &str) -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let profile_path = texman_dir.join("profiles").join(name);
    let active_path = texman_dir.join("active");

    if !profile_path.exists() {
        anyhow::bail!("Profile '{}' does not exist.", name);
    }

    if active_path.exists() && active_path.read_link()?.file_name().unwrap().to_str().unwrap() == name {
        anyhow::bail!("Cannot remove active profile '{}'. Switch to another profile first.", name);
    }

    fs::remove_dir_all(&profile_path)?;
    let conn = init_db(&texman_dir)?;
    conn.execute(
        "DELETE FROM installed_packages WHERE profile = ?1",
        params![name],
    )?;
    log::info!("Removed profile '{}'", name);

    Ok(())
}

fn copy_recursively(source: &PathBuf, destination: &PathBuf) -> anyhow::Result<()> {
    if source.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let src_path = entry.path();
            let dest_path = destination.join(entry.file_name());
            copy_recursively(&src_path, &dest_path)?;
        }
    } else {
        fs::copy(source, destination)?;
    }
    Ok(())
}

fn backup_profile(name: &str) -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let active_path = texman_dir.join("active");

    if !active_path.exists() {
        anyhow::bail!("No active profile set. Install a package or switch to a profile first.");
    }

    let active_dir = fs::canonicalize(&active_path)?;
    let active_profile = active_path.read_link()?
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let backup_dir = texman_dir.join("backups").join(name);
    std::fs::create_dir_all(&backup_dir)?;

    for entry in fs::read_dir(&active_dir)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = backup_dir.join(entry.file_name());
        copy_recursively(&src_path, &dest_path)?;
    }

    let conn = init_db(&texman_dir)?;
    let mut stmt = conn.prepare("SELECT name, revision FROM installed_packages WHERE profile = ?1")?;
    let rows = stmt.query_map(params![active_profile], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (pkg_name, revision) = row?;
        conn.execute(
            "INSERT INTO backups (backup_name, profile, name, revision) VALUES (?1, ?2, ?3, ?4)",
            params![name, active_profile, pkg_name, revision],
        )?;
    }

    log::info!("Created backup '{}' for profile '{}'", name, active_profile);
    Ok(())
}

fn restore_profile(name: &str) -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let active_path = texman_dir.join("active");
    let backup_dir = texman_dir.join("backups").join(name);

    if !active_path.exists() {
        anyhow::bail!("No active profile set. Install a package or switch to a profile first.");
    }
    if !backup_dir.exists() {
        anyhow::bail!("Backup '{}' does not exist.", name);
    }

    let active_dir = fs::canonicalize(&active_path)?;
    let active_profile = active_path.read_link()?
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    for entry in fs::read_dir(&active_dir)? {
        let entry = entry?;
        if entry.path().is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }

    for entry in fs::read_dir(&backup_dir)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = active_dir.join(entry.file_name());
        copy_recursively(&src_path, &dest_path)?;
    }

    let conn = init_db(&texman_dir)?;
    conn.execute(
        "DELETE FROM installed_packages WHERE profile = ?1",
        params![active_profile],
    )?;
    let mut stmt = conn.prepare("SELECT name, revision FROM backups WHERE backup_name = ?1")?;
    let rows = stmt.query_map(params![name], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (pkg_name, revision) = row?;
        conn.execute(
            "INSERT INTO installed_packages (profile, name, revision) VALUES (?1, ?2, ?3)",
            params![active_profile, pkg_name, revision],
        )?;
    }

    log::info!("Restored profile '{}' from backup '{}'", active_profile, name);
    Ok(())
}

fn list_backups() -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let conn = init_db(&texman_dir)?;

    let mut stmt = conn.prepare("SELECT backup_name, MIN(created_at), COUNT(name) FROM backups GROUP BY backup_name ORDER BY backup_name")?;
    let backups = stmt.query_map([], |row| {
        let name: String = row.get(0)?;
        let timestamp: i64 = row.get(1)?;
        let pkg_count: i64 = row.get(2)?;
        Ok((name, timestamp, pkg_count))
    })?;

    let mut backup_list = Vec::new();
    for backup in backups {
        backup_list.push(backup?);
    }

    if backup_list.is_empty() {
        println!("No backups found.");
        return Ok(());
    }

    println!("Available backups:");
    for (name, timestamp, pkg_count) in backup_list {
        let dt = DateTime::<Utc>::from_timestamp(timestamp, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string();
        println!("  {} (created: {}, packages: {})", name, dt, pkg_count);
    }

    Ok(())
}

fn remove_backup(name: &str) -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let backup_dir = texman_dir.join("backups").join(name);

    if !backup_dir.exists() {
        anyhow::bail!("Backup '{}' does not exist.", name);
    }

    fs::remove_dir_all(&backup_dir)?;
    let conn = init_db(&texman_dir)?;
    conn.execute("DELETE FROM backups WHERE backup_name = ?1", params![name])?;
    log::info!("Removed backup '{}'", name);

    Ok(())
}

fn clean(remove_backups: bool) -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");

    let mut removed_files = 0;
    for entry in fs::read_dir(&texman_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("xz") {
            fs::remove_file(&path)?;
            removed_files += 1;
            log::debug!("Removed unused file: {:?}", path);
        }
    }
    log::info!("Removed {} unused .tar.xz files", removed_files);

    if remove_backups {
        let backups_dir = texman_dir.join("backups");
        if backups_dir.exists() {
            fs::remove_dir_all(&backups_dir)?;
            fs::create_dir_all(&backups_dir)?;
            let conn = init_db(&texman_dir)?;
            conn.execute("DELETE FROM backups", [])?;
            log::info!("Removed all backups");
        } else {
            log::info!("No backups to remove");
        }
    }

    Ok(())
}
