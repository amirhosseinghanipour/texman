use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
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

#[derive(Serialize, Deserialize, Debug)]
struct Package {
    name: String,
    version: String,
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
    log::info!("Fetching TLPDB from CTAN mirror");
    let mock_response = r#"
    {
        "babel": {
            "name": "babel",
            "version": "3.9",
            "url": "http://mirror.ctan.org/systems/texlive/tlnet/archive/babel.tar.xz",
            "depends": ["texlive-core"]
        },
        "fontspec": {
            "name": "fontspec",
            "version": "2.7",
            "url": "http://mirror.ctan.org/systems/texlive/tlnet/archive/fontspec.tar.xz",
            "depends": ["texlive-core", "xetex"]
        },
        "texlive-core": {
            "name": "texlive-core",
            "version": "2023",
            "url": "http://mirror.ctan.org/systems/texlive/tlnet/archive/texlive-core.tar.xz",
            "depends": []
        },
        "xetex": {
            "name": "xetex",
            "version": "0.9999",
            "url": "http://mirror.ctan.org/systems/texlive/tlnet/archive/xetex.tar.xz",
            "depends": ["texlive-core"]
        }
    }"#;

    let packages: HashMap<String, Package> = serde_json::from_str(mock_response)?;
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
    log::info!("Downloading {} v{} from {}", pkg.name, pkg.version, pkg.url);
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

        let store_path = texman_dir.join("store").join(format!("{}-{}", pkg.name, pkg.version));
        std::fs::create_dir_all(&store_path)?;

        log::info!("Installing {} v{} to {:?}", pkg.name, pkg.version, store_path);
        let tar_gz = File::open(&download_path)?;
        let tar = GzDecoder::new(tar_gz);
        let mut archive = tar::Archive::new(tar);
        archive.unpack(&store_path)?;

        std::fs::remove_file(&download_path)?;
        log::info!("Installed {} v{}", pkg.name, pkg.version);
    }

    Ok(())
}
