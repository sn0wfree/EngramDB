# 在 WSL Ubuntu 中安装构建依赖 + Rust（在 Windows 管理员执行 wsl_install.ps1 后运行）
# 也可直接在 WSL 里 bash 跑 scripts/wsl_init_rust.sh
$ErrorActionPreference = 'Stop'
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
# 项目在 /mnt/d/Github/EngramDB
cd /mnt/d/Github/EngramDB
echo "Ready. Run: cd /mnt/d/Github/EngramDB && cargo test --test differential_sqlite"
'@
$shPath = 'D:\Github\EngramDB\scripts\wsl_init_rust.sh'
Set-Content -Path $shPath -Value $sh -Encoding UTF8 -NoNewline
wsl.exe -d Ubuntu -u root -- bash /mnt/d/Github/EngramDB/scripts/wsl_init_rust.sh 2>&1
