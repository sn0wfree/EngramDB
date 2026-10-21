# Install build deps + Rust inside WSL Ubuntu (run after wsl_install.ps1)
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File scripts\wsl_init_rust.ps1
$ErrorActionPreference = 'Stop'
$shPath = Join-Path $PSScriptRoot 'wsl_init_rust.sh'
$sh = @'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -y
sudo apt-get install -y build-essential pkg-config libssl-dev curl git python3
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi
source "$HOME/.cargo/env"
rustc --version
cargo --version
cd /mnt/d/Github/EngramDB
echo "Ready. Example: cd /mnt/d/Github/EngramDB && cargo test --test differential_sqlite"
'@
# UTF-8 no BOM is fine for bash
[System.IO.File]::WriteAllText($shPath, $sh)
wsl.exe -d Ubuntu -- bash /mnt/d/Github/EngramDB/scripts/wsl_init_rust.sh
