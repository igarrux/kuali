# Choose and run a project command interactively. Falls back to a plain list.
_default:
    @if command -v fzf >/dev/null 2>&1; then \
        just --choose --chooser 'fzf --height=80% --layout=reverse --border --prompt="Kuali › " --preview="just --show {}" --preview-window="right:60%:wrap"'; \
    else \
        just --list; \
    fi

# Show the versions of the tools used by the project.
[group('development')]
doctor:
    @just --version
    @fzf --version
    @cargo --version
    @cargo tauri --version
    @node --version
    @npm --version
    @python3 --version

# Run the Kuali desktop app in development mode.
[group('development')]
dev:
    cargo run -p kuali-app

# Compile the complete Rust workspace without creating app bundles.
[group('development')]
build:
    cargo build --workspace

# Format every Rust crate in place.
[group('quality')]
format:
    cargo fmt --all

# Verify Rust formatting without changing files.
[group('quality')]
format-check:
    cargo fmt --all --check

# Run Clippy across every crate and target, treating warnings as errors.
[group('quality')]
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Run all Rust unit, integration, and documentation tests.
[group('tests')]
test-rust:
    cargo test --workspace

# Test the desktop UI translations and behavioral contracts.
[group('tests')]
test-ui:
    node --test src/i18n.test.mjs

# Validate the website, SEO metadata, links, and deployment files.
[group('tests')]
test-website:
    node --test website/tests/site.test.mjs

# Run the browser extension test suite.
[group('tests')]
test-extension:
    npm --prefix browser-extension test

# Run every automated test suite in the repository.
[group('tests')]
test:
    just test-rust
    just test-ui
    just test-website
    just test-extension

# Run the complete local validation, including the store package.
[group('quality')]
check:
    just format-check
    just lint
    just test
    just extension-package

# Build the minimal Chrome Web Store ZIP and checksum under dist.
[group('browser extension')]
extension-package:
    npm --prefix browser-extension run package:store

# Run the full interactive Google Meet end-to-end test.
[group('browser extension')]
meet-e2e:
    npm --prefix browser-extension run test:e2e:meet

# Run the one-person Google Meet end-to-end smoke test.
[group('browser extension')]
meet-e2e-solo:
    npm --prefix browser-extension run test:e2e:meet -- --solo

# Serve the static website locally at http://127.0.0.1:4173.
[group('website')]
website-serve:
    python3 -m http.server 4173 --bind 127.0.0.1 --directory website

# Build local desktop bundles and updater artifacts with Tauri.
[group('packaging')]
bundle:
    cargo tauri build
