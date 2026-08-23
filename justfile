default: run

# Privileged development using a transient systemd helper; no installation.
run:
    cargo run -r

# Explicit unprivileged fallback for systems without systemd/PolicyKit.
run-unprivileged:
    cargo run -r --no-default-features

# Command-line client using the same transient development helper.
list:
    cargo run -r -p defragger-cli -- list

analyze device:
    cargo run -r -p defragger-cli -- analyze {{device}}

defrag device:
    cargo run -r -p defragger-cli -- defrag {{device}} --yes --require-fully-defragmented

check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace
    cargo test --workspace --all-features

# Root-capability ext4 loop-image test; the script elevates only required steps.
integration-test:
    crates/defrag-service/tests/fixtures/run-ext4-fixture.sh

# Build the production split-service configuration without installing it.
system-build:
    cmake -S . -B build/system -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr
    cmake --build build/system

# Install the production GUI, PolicyKit action, and system helper.
system-install: system-build
    sudo cmake --install build/system
    sudo systemctl daemon-reload
