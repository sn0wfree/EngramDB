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