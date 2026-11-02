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

# Build all release binaries and package into dist/ folder (matching go2rtc release format)
dist: mac linux-amd64 linux-arm64
    @rm -rf dist
    @mkdir -p dist
    @cp target/release/rust2rtc dist/rust2rtc_mac_arm64
    @cp target/x86_64-unknown-linux-musl/release/rust2rtc dist/rust2rtc_linux_amd64
    @cp target/aarch64-unknown-linux-musl/release/rust2rtc dist/rust2rtc_linux_arm64
    @mkdir -p dist/zip_tmp
    @cp target/release/rust2rtc dist/zip_tmp/rust2rtc
    @cd dist/zip_tmp && zip -9 ../rust2rtc_mac_arm64.zip rust2rtc
    @rm -rf dist/zip_tmp
    @chmod +x dist/rust2rtc_*
    @echo "\n📦 Release binaries packaged in ./dist (matching go2rtc release layout):"
    @ls -lh dist/
