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
        name: String,
    },
    Restore {
        name: String,
    },
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
}

#[derive(Subcommand)]
enum ProfileAction {
    Create {
        name: String,
    },
    Switch {
        name: String,
    },
}

#[derive(Debug, Clone)]
struct Package {
    name: String,
    revision: String,
    url: String,
    depends: Vec<String>,
    runfiles: Vec<String>,
    binfiles: Vec<String>,
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
        Commands::Backup { name } => {
            log::info!("Backing up active profile to '{}'", name);
            backup_profile(&name)?;
        }
        Commands::Restore { name } => {
            log::info!("Restoring active profile from backup '{}'", name);
            restore_profile(&name)?;
        }
        Commands::Profile { action } => match action {
            ProfileAction::Create { name } => create_profile(&name)?,
            ProfileAction::Switch { name } => switch_profile(&name)?,
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

    parse_tlpdb(&tlpdb_text)
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
    let mut packages = HashMap::new();
    let mut current_pkg: Option<(String, Vec<String>, String, Vec<String>, Vec<String>)> = None;

    let mut in_runfiles = false;
    let mut in_binfiles = false;

    for line in tlpdb_text.lines() {
        if line.is_empty() && current_pkg.is_some() {
            let (name, depends, revision, runfiles, binfiles) = current_pkg.take().unwrap();
            let url = format!(
                "http://mirror.ctan.org/systems/texlive/tlnet/archive/{}.tar.xz",
                name
            );
            packages.insert(
                name.clone(),
                Package {
                    name,
                    revision,
                    url,
                    depends,
                    runfiles,
                    binfiles,
                },
            );
            in_runfiles = false;
            in_binfiles = false;
        } else if line.starts_with("name ") {
            if let Some((name, _, _, _, _)) = current_pkg.take() {
                let url = format!(
                    "http://mirror.ctan.org/systems/texlive/tlnet/archive/{}.tar.xz",
                    name
                );
                packages.insert(
                    name.clone(),
                    Package {
                        name,
                        revision: "unknown".to_string(),
                        url,
                        depends: vec![],
                        runfiles: vec![],
                        binfiles: vec![],
                    },
                );
            }
            let name = line.strip_prefix("name ").unwrap().to_string();
            current_pkg = Some((name, vec![], "unknown".to_string(), vec![], vec![]));
            in_runfiles = false;
            in_binfiles = false;
        } else if let Some((_, depends, revision, runfiles, binfiles)) = &mut current_pkg {
            if line == "runfiles" {
                in_runfiles = true;
                in_binfiles = false;
            } else if line == "binfiles" {
                in_runfiles = false;
                in_binfiles = true;
            } else if line.starts_with("depends ") {
                let deps = line.strip_prefix("depends ").unwrap();
                if !deps.is_empty() {
                    depends.extend(deps.split(',').map(|s| s.trim().to_string()));
                }
                in_runfiles = false;
                in_binfiles = false;
            } else if line.starts_with("revision ") {
                *revision = line.strip_prefix("revision ").unwrap().to_string();
                in_runfiles = false;
                in_binfiles = false;
            } else if in_runfiles && line.starts_with(" ") {
                runfiles.push(line.trim().to_string());
            } else if in_binfiles && line.starts_with(" ") {
                binfiles.push(line.trim().to_string());
            } else {
                in_runfiles = false;
                in_binfiles = false;
            }
        }
    }

    if let Some((name, depends, revision, runfiles, binfiles)) = current_pkg {
        let url = format!(
            "http://mirror.ctan.org/systems/texlive/tlnet/archive/{}.tar.xz",
            name
        );
        packages.insert(
            name.clone(),
            Package {
                name,
                revision,
                url,
                depends,
                runfiles,
                binfiles,
            },
        );
    }

    log::info!("Parsed {} packages from TLPDB", packages.len());
    Ok(packages)
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
