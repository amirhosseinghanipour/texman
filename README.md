# TexMan: A Rust-Based Package Manager for LaTeX

`texman` is a lightweight, command-line package manager for TeX Live, written in Rust. It allows you to install, update, remove, and manage LaTeX packages efficiently across multiple profiles, with support for backups and detailed package searches.

## Features

- **Package Management**: Install, update, remove, and list LaTeX packages from the TeX Live repository.
- **Profiles**: Create and switch between isolated TeX environments (profiles).
- **Backups**: Create, list, restore, and remove backups of your active profile with timestamps and package counts.
- **Search**: Search packages by name, short description, long description, or dependencies.
- **Clean**: Remove unused downloaded files and optionally all backups.
- **Parallel Parsing**: Fast TLPDB parsing with `rayon` for multi-core performance.
- **Incremental Parsing**: Caches parsed TLPDB data for quick startup.

## Installation

### Prerequisites
- Rust (1.70+ recommended) and Cargo (install via [rustup](https://rustup.rs/)).
- A working internet connection to fetch the TeX Live package database (TLPDB).

### Pre-built Binaries
Download the appropriate binary for your platform from the [Releases](https://github.com/amirhosseinghanipour/texman/releases) page:
- **macOS** (x86_64): `texman-macos-x86_64`
- **Linux** (x86_64, glibc): `texman-linux-x86_64`
- **Linux** (x86_64, musl): `texman-linux-x86_64-musl`

Extract and move to a directory in your PATH:
```bash
chmod +x texman-<platform>
sudo mv texman-<platform> /usr/local/bin/texman
```

### Build from Source
1. Clone the repository:
```bash
git clone https://github.com/amirhosseinghanipour/texman.git
cd texman
```
2. Build and install:
```bash
cargo build --release
sudo cp target/release/texman /usr/local/bin/
```

## Usage

### Basic Commands
- Install a package:
```bash
texman install babel --profile minimal
```
- List installed packages:
```bash
texman list
```
- Update packages:
```bash
texman update
```
- Remove a package:
```bash
texman remove babel
```
- Get package info:
```bash
texman info babel
```

### Profile Management
- Create a profile:
```bash
texman profile create myprofile
```
- Switch profiles:
```bash
texman profile switch myprofile
```
- List profiles:
```bash
texman profile list
```
- Remove a profile:
```bash
texman proile remove myprofile
```

### Backup Management
- Create a backup:
```bash
texman backup create mybackup
```
- List backups:
```bash
texman backup list
```
- Restore a backup:
```bash
texman restore mybackup
```
- Remove a backup:
```bash
texman backup remove mybacup
```

### Search
- Search by name:
```bash
texman search latex
```
- Search with descriptions or dependencies:
```bash
texman search latex --description --longdesc --depends
```

### Cleaup
- Remove unused files:
```bash
texman clean
```
- Remove all backups too:
```bash
texman clean --backups
```

## Configuration
- Storage: Packages, profiles, and backups are stored in ~/.texman/.
- Database: SQLite database at ~/.texman/db/texman.sqlite tracks installed packages and backups.
- TLPDB Cache: Cached at ~/.texman/db/tlpdb.txt and tlpdb.bin, refreshed every 24 hours.

## Supported Platforms
- macOS (x86_64)
- Linux (x86_64, glibc and musl-based distros like Arch, Ubuntu, Fedora)

## Contributing
Contributions are welcome! Please submit issues or pull requests to GitHub.
