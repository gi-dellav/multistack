# Justfile
# https://github.com/casey/just

[private]
default:
    @just --list

# ---- Build ----

build:
    cargo build --release

run *args:
    cargo run -- {{ args }}

# ---- Quality ----

fmt:
    cargo fmt
    cargo clippy --all-targets -- -D warnings

check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

test: fmt
    cargo test

# ---- Tags ----

add-tag:
    #!/usr/bin/env bash
    set -euo pipefail
    git push origin main
    VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
    git tag -a "v${VERSION}" -m "Release v${VERSION}"
    git push origin "v${VERSION}"
    echo "Created and pushed tag v${VERSION}"

# ---- Packaging: version sync ----

sync-version:
    bash scripts/sync-version.sh

# ---- Packaging: checksums ----

homebrew-checksums:
    #!/usr/bin/env bash
    set -euo pipefail
    VERSION=$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
    echo "Computing SHA256 sums for v${VERSION}..."

    SHA_DARWIN_X86=$(curl -sL "https://github.com/gi-dellav/multistack/releases/download/v${VERSION}/multistack-x86_64-apple-darwin.tar.gz" | sha256sum | cut -d' ' -f1)
    SHA_DARWIN_ARM=$(curl -sL "https://github.com/gi-dellav/multistack/releases/download/v${VERSION}/multistack-aarch64-apple-darwin.tar.gz" | sha256sum | cut -d' ' -f1)
    SHA_LINUX_X86=$(curl -sL "https://github.com/gi-dellav/multistack/releases/download/v${VERSION}/multistack-x86_64-unknown-linux-musl.tar.gz" | sha256sum | cut -d' ' -f1)
    SHA_LINUX_ARM=$(curl -sL "https://github.com/gi-dellav/multistack/releases/download/v${VERSION}/multistack-aarch64-unknown-linux-musl.tar.gz" | sha256sum | cut -d' ' -f1)

    sed -i "/multistack-x86_64-apple-darwin.tar.gz/{n;s/sha256 \".*\"/sha256 \"${SHA_DARWIN_X86}\"/}" packaging/homebrew/multistack.rb
    sed -i "/multistack-aarch64-apple-darwin.tar.gz/{n;s/sha256 \".*\"/sha256 \"${SHA_DARWIN_ARM}\"/}" packaging/homebrew/multistack.rb
    sed -i "/multistack-x86_64-unknown-linux-musl.tar.gz/{n;s/sha256 \".*\"/sha256 \"${SHA_LINUX_X86}\"/}" packaging/homebrew/multistack.rb
    sed -i "/multistack-aarch64-unknown-linux-musl.tar.gz/{n;s/sha256 \".*\"/sha256 \"${SHA_LINUX_ARM}\"/}" packaging/homebrew/multistack.rb

    echo "Updated SHA256 sums in packaging/homebrew/multistack.rb"

# ---- Packaging: release workflow ----

pre-release: sync-version
    @echo "=== pre-release done: version synced across all packaging files ==="
    @echo "Next: just add-tag, wait for GitHub release, then: just post-release"

post-release: homebrew-checksums
    @echo "=== post-release done: all checksums updated ==="
    @echo "Ready for:"
    @echo "  homebrew: push packaging/homebrew/multistack.rb to homebrew-tap repo"
