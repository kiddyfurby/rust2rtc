# Default recipe lists all available commands
default:
    @just --list

# Run local development server
dev:
    cargo run

# Run production release binary on macOS
start:
    cargo run --release

# Run all test suites
test:
    cargo test

# Quick compiler check
check:
    cargo check

# -----------------------------------------------------------------------------
# Release Builds
# -----------------------------------------------------------------------------

# Build macOS ARM64 release binary (Apple Silicon)
mac:
    cargo build --release
    @echo "✅ macOS ARM64 binary built: target/release/rust2rtc"

# Build Linux AMD64 (x86_64) release binary (Statically linked musl)
linux-amd64:
    cargo zigbuild --target x86_64-unknown-linux-musl --release
    @echo "✅ Linux x86_64 binary built: target/x86_64-unknown-linux-musl/release/rust2rtc"

# Build Linux ARM64 (aarch64 / Raspberry Pi / Home Assistant) release binary (Statically linked musl)
linux-arm64:
    cargo zigbuild --target aarch64-unknown-linux-musl --release
    @echo "✅ Linux ARM64 binary built: target/aarch64-unknown-linux-musl/release/rust2rtc"

# Build all release binaries and package into dist/ folder
dist: mac linux-amd64 linux-arm64
    @mkdir -p dist
    @cp target/release/rust2rtc dist/rust2rtc_mac_arm64
    @cp target/x86_64-unknown-linux-musl/release/rust2rtc dist/rust2rtc_linux_amd64
    @cp target/aarch64-unknown-linux-musl/release/rust2rtc dist/rust2rtc_linux_arm64
    @cd dist && zip -9 rust2rtc_mac_arm64.zip rust2rtc_mac_arm64
    @cd dist && tar -czvf rust2rtc_linux_amd64.tar.gz rust2rtc_linux_amd64
    @cd dist && tar -czvf rust2rtc_linux_arm64.tar.gz rust2rtc_linux_arm64
    @cd dist && shasum -a 256 rust2rtc_* > checksums.sha256
    @echo "\n📦 All release binaries packaged in ./dist:"
    @ls -lh dist/
