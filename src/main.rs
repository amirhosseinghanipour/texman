use clap::{Parser, Subcommand};
use std::collections::HashMap;
use flate2::read::GzDecoder;
use std::fs::File;
use std::path::PathBuf;

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
    },
}

#[derive(Debug)]
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
        Commands::Install { package } => {
            log::info!("Installing package: {}", package);
            install_package(&package, &tlpdb).await?;
        }
    }

    Ok(())
}

async fn fetch_tlpdb() -> anyhow::Result<HashMap<String, Package>> {
    let tlpdb_text = fetch_tlpdb_text().await?;
    parse_tlpdb(&tlpdb_text)
}

async fn fetch_tlpdb_text() -> anyhow::Result<String> {
    log::info!("Fetching TLPDB from CTAN mirror");
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

async fn install_package(package: &str, tlpdb: &HashMap<String, Package>) -> anyhow::Result<()> {
    let texman_dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
        .join(".texman");
    std::fs::create_dir_all(&texman_dir)?;

    let mut to_install = Vec::new();
    resolve_dependencies(package, tlpdb, &mut to_install)?;
    log::info!("Packages to install: {:?}", to_install);

    for pkg_name in to_install {
        let pkg = tlpdb.get(&pkg_name).unwrap();
        let download_path = download_package(pkg, &texman_dir).await?;

        let store_path = texman_dir.join("store").join(format!("{}-r{}", pkg.name, pkg.revision));
        std::fs::create_dir_all(&store_path)?;

        log::info!("Installing {} r{} to {:?}", pkg.name, pkg.revision, store_path);
        let tar_gz = File::open(&download_path)?;
        let tar = GzDecoder::new(tar_gz);
        let mut archive = tar::Archive::new(tar);
        archive.unpack(&store_path)?;

        std::fs::remove_file(&download_path)?;
        log::info!("Installed {} r{}", pkg.name, pkg.revision);
    }

    Ok(())
}
