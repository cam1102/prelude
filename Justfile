set shell := ["bash", "-euo", "pipefail", "-c"]

# Top-level image and workspace orchestration.
default:
    @just --list

# ---- Daemon (Rust) ------------------------------------------------------

# Cross-compile the daemon for aarch64 Car Thing hardware.
daemon-build:
    cross build --locked -p preluded --target=aarch64-unknown-linux-gnu --release

# Build the daemon for the development host.
daemon-host:
    cargo build --locked -p preluded

# ---- Workspace build ----------------------------------------------------

build:
    cargo build --locked --workspace

build-release:
    cargo build --locked --workspace --release

# ---- Image (Yocto) ------------------------------------------------------

image_dir := justfile_directory() / "image"

[macos]
image-macos-setup:
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" macos-setup

image-checkout profile="prelude-prod-local":
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" checkout {{ quote(profile) }}

image-parse profile="prelude-prod-local":
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" parse {{ quote(profile) }}

image-shell profile="prelude-prod-local":
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" shell {{ quote(profile) }}

image-build profile="prelude-prod-local":
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" build {{ quote(profile) }}

image-flash image="prelude-prod-image":
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" flash {{ quote(image) }}

image-flash-env image="prelude-prod-image":
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" flash-env {{ quote(image) }}

[macos]
image-pull-deploy:
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" pull-deploy

image-clean-build:
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" clean-build

image-vscode-setup:
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" vscode-setup

image-push-sstate:
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" push-sstate

image-lint:
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" lint

image-test:
    just --justfile "{{ image_dir }}/Justfile" --working-directory "{{ image_dir }}" test

# ---- Workspace lint / test ----------------------------------------------

test:
    cargo test --locked --workspace --all-features

lint:
    cargo fmt --all --check
    cargo clippy --locked --workspace --all-targets --all-features -- -D warnings

fmt:
    cargo fmt --all

check: lint test image-lint
