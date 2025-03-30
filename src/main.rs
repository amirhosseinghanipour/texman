use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use chrono::{DateTime, Utc, Duration};
use std::fs;
use futures::future::join_all;
use xz2::read::XzDecoder;
use tar;

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
        Commands::Profile { action } => match action {
            ProfileAction::Create { name } => create_profile(&name)?,
            ProfileAction::Switch { name } => switch_profile(&name)?,
        },
    }

    Ok(())
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
    let tlpdb_text = response.text().await?;
    log::debug!("Fetched TLPDB ({} bytes)", tlpdb_text.len());
    Ok(tlpdb_text)
}

fn parse_tlpdb(tlpdb_text: &str) -> anyhow::Result<HashMap<String, Package>> {
    let mut packages = HashMap::new();
    let mut current_pkg: Option<(String, Vec<String>, String)> = None;

    for line in tlpdb_text.lines() {
        if line.is_empty() && current_pkg.is_some() {
            let (name, depends, revision) = current_pkg.take().unwrap();
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
                },
            );
        } else if line.starts_with("name ") {
            if let Some((name, _, _)) = current_pkg.take() {
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
                    },
                );
            }
            let name = line.strip_prefix("name ").unwrap().to_string();
            current_pkg = Some((name, vec![], "unknown".to_string()));
        } else if let Some((_, depends, revision)) = &mut current_pkg {
            if line.starts_with("depends ") {
                let deps = line.strip_prefix("depends ").unwrap();
                if !deps.is_empty() {
                    depends.extend(deps.split(',').map(|s| s.trim().to_string()));
                }
            } else if line.starts_with("revision ") {
                *revision = line.strip_prefix("revision ").unwrap().to_string();
            }
        }
    }

    if let Some((name, depends, revision)) = current_pkg {
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
) -> anyhow::Result<()> {
    let pkg = tlpdb.get(package).ok_or_else(|| anyhow::anyhow!("Package {} not found", package))?;

    for dep in &pkg.depends {
        if !resolved.contains(dep) {
            log::debug!("Resolving dependency: {}", dep);
            resolve_dependencies(dep, tlpdb, resolved)?;
            resolved.push(dep.clone());
        }
    }

    if !resolved.contains(&pkg.name) {
        resolved.push(pkg.name.clone());
    }

    Ok(())
}

async fn download_package(pkg: &Package, texman_dir: &PathBuf) -> anyhow::Result<PathBuf> {
    let download_path = texman_dir.join(format!("{}.tar.xz", pkg.name));
    log::info!("Downloading {} r{} from {}", pkg.name, pkg.revision, pkg.url);
    let response = reqwest::get(&pkg.url).await?;
    let bytes = response.bytes().await?;
    std::fs::write(&download_path, bytes)?;
    Ok(download_path)
}

async fn install_package(package: &str, profile: &str, tlpdb: &HashMap<String, Package>) -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    let profile_dir = texman_dir.join("profiles").join(profile);
    std::fs::create_dir_all(&profile_dir)?;

    let mut to_install = Vec::new();
    resolve_dependencies(package, tlpdb, &mut to_install)?;
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
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

    for (pkg, download_path) in packages.iter().zip(download_paths.iter()) {
        let store_path = profile_dir.join(format!("{}-r{}", pkg.name, pkg.revision));
        std::fs::create_dir_all(&store_path)?;

        log::info!("Installing {} r{} to {:?}", pkg.name, pkg.revision, store_path);
        let tar_xz = File::open(download_path)?;
        let tar = XzDecoder::new(tar_xz);
        let mut archive = tar::Archive::new(tar);
        archive.unpack(&store_path)?;

        std::fs::remove_file(download_path)?;
        log::info!("Installed {} r{}", pkg.name, pkg.revision);
    }

    let active_path = texman_dir.join("active");
    if !active_path.exists() {
        std::os::unix::fs::symlink(&profile_dir, &active_path)?;
        log::info!("Set {} as active profile", profile);
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
        anyhow::bail!("Profile {} does not exist", name);
    }

    if active_path.exists() {
        std::fs::remove_file(&active_path)?;
    }
    std::os::unix::fs::symlink(&profile_path, &active_path)?;
    log::info!("Switched to profile: {}", name);
    Ok(())
}
