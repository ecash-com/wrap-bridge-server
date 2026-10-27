#!/bin/bash
# Build the offline install bundle for the ECX bridge.
#
# Run this on a machine WITH internet access, running the same Ubuntu release and architecture as
# the bridge server (Ubuntu 24.04, x86_64 or arm64), for example a throwaway Multipass VM. It
# produces ./bundle/, about 2.5 GB, which install.sh installs from without touching the internet.
# Copy the repository together with bundle/ to the server, on a USB stick if you like.
#
# The bundle contains:
#   debs/            runtime packages and their dependencies, as .deb files
#   node.tar.xz      Node.js, hash-checked against the pin in install.sh
#   swap-backend     the backend, built --release here so the server needs no compiler
#   frontend/        the built frontend and its node_modules
#   cloudflared.deb  Cloudflare's connector
#   llama-server     llama.cpp's server, built from the pinned commit, for the local assistant
#   model/           the pinned model and its license, if present or downloadable
#   MANIFEST         SHA-256 of every file above; install.sh refuses a bundle that does not match
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUNDLE="$ROOT/bundle"
NODE_VERSION="v22.23.4"
NODE_SHA_X64="1805bcd35e6815559bd2eb97db438d3fdb0eb3a5e6a977b7c1badca76337e9ce"
NODE_SHA_ARM64="6287aa46b6c375d816e63ae66d024fbbfd70404825aabdfdf898a970871d2f4b"
# Packages the server needs at runtime. Not build tools: the backend and frontend arrive built.
# curl is used by install.sh for the egress check and health checks; xz-utils unpacks Node.
RUNTIME_PACKAGES=(wireguard wireguard-tools curl ca-certificates xz-utils systemd-resolved nftables rsync python3)
SKIP_MODEL=0
SKIP_ASSISTANT=0

die() { printf 'make-bundle.sh: %s\n' "$1" >&2; exit 1; }
step() { printf '\n==> %s\n' "$1"; }

usage() {
	cat <<'EOF'
Usage: ./make-bundle.sh [--no-model] [--no-assistant]

  --no-model      do not fetch the assistant's model; install.sh will still install the assistant
                  if bundle/model/ is filled in by hand before copying
  --no-assistant  leave out llama-server and the model entirely
EOF
}

for arg in "$@"; do
	case "$arg" in
		--no-model) SKIP_MODEL=1 ;;
		--no-assistant) SKIP_ASSISTANT=1; SKIP_MODEL=1 ;;
		-h|--help) usage; exit 0 ;;
		*) usage; die "unknown argument: $arg" ;;
	esac
done

require_build_host() {
	[[ -r /etc/os-release ]] || die "this script is for Ubuntu 24.04"
	# shellcheck disable=SC1091
	. /etc/os-release
	[[ "${ID:-}" == "ubuntu" && "${VERSION_ID:-}" == 24.* ]] || die "run this on Ubuntu 24.04, the release the server runs; found ${PRETTY_NAME:-unknown}. The .deb files and the built binaries are tied to it."
	case "$(uname -m)" in
		x86_64) ARCH=x64; DEB_ARCH=amd64; NODE_SHA="$NODE_SHA_X64" ;;
		aarch64) ARCH=arm64; DEB_ARCH=arm64; NODE_SHA="$NODE_SHA_ARM64" ;;
		*) die "unsupported architecture: $(uname -m)" ;;
	esac
	[[ "$(id -u)" -eq 0 ]] || die "run with sudo: apt-get needs root to install the build tools here"
}

install_build_tools() {
	step "Installing build tools on this machine (not on the server)"
	export DEBIAN_FRONTEND=noninteractive
	apt-get update
	apt-get install -y build-essential pkg-config libssl-dev cmake git curl ca-certificates gnupg xz-utils
	if ! command -v cargo >/dev/null && [[ ! -x /root/.cargo/bin/cargo ]]; then
		curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable >/dev/null
	fi
	# rustup's env script reads $HOME, which a non-login root shell (sudo, systemd-run) may not set.
	export HOME="${HOME:-/root}"
	# shellcheck disable=SC1091
	[[ -f /root/.cargo/env ]] && . /root/.cargo/env
	if [[ ! -x /usr/local/bin/node ]]; then
		install_node_locally
	fi
	/usr/local/bin/npm install -g pnpm@10 >/dev/null
}

fetch_node() {
	step "Fetching Node.js $NODE_VERSION"
	local file="node-${NODE_VERSION}-linux-${ARCH}.tar.xz"
	curl -fsSL "https://nodejs.org/dist/${NODE_VERSION}/${file}" -o "$BUNDLE/node.tar.xz"
	printf '%s  %s\n' "$NODE_SHA" "$BUNDLE/node.tar.xz" | sha256sum -c - >/dev/null || die "Node tarball hash mismatch"
}

install_node_locally() {
	[[ -f "$BUNDLE/node.tar.xz" ]] || fetch_node
	tar -xJf "$BUNDLE/node.tar.xz" -C /usr/local --strip-components=1
}

fetch_debs() {
	step "Downloading runtime packages and their dependencies"
	install -d "$BUNDLE/debs"
	rm -f "$BUNDLE"/debs/*.deb
	# The full dependency closure, Essential and required packages included: a bundled package may
	# need a newer libc6 than the server's image has, and offline there is nowhere else to get it.
	# install.sh skips every package the server already has at the same or a newer version, so
	# shipping the base libraries costs disk space on the stick and nothing on the server.
	local pkg closure
	closure="$(apt-cache depends --recurse --no-recommends --no-suggests --no-conflicts --no-breaks \
		--no-replaces --no-enhances "${RUNTIME_PACKAGES[@]}" \
		| awk '/^[^ <]/ {print $1} /^ *\|?(Pre)?Depends: [^<]/ {print $2}' | sort -u)"
	local wanted=()
	for pkg in $closure; do
		# Whole output, then the first stanza only: piping into head would close the pipe early,
		# fail under pipefail, and silently drop any package that has several versions on offer.
		local info
		info="$(apt-cache show "$pkg" 2>/dev/null)" || continue
		info="$(awk '/^Package:/ {n++} n == 1' <<<"$info")"
		[[ -n "$info" ]] || continue
		wanted+=("$pkg")
	done
	(cd "$BUNDLE/debs" && apt-get download "${wanted[@]}")
	# Every package we meant to include must be here; a silent miss would surface only on the
	# server, where there is no way to fetch it.
	local missing=0
	for pkg in "${wanted[@]}"; do
		ls "$BUNDLE/debs/${pkg}_"*.deb >/dev/null 2>&1 || { printf 'missing: %s\n' "$pkg"; missing=1; }
	done
	((missing == 0)) || die "some packages were not downloaded"
	printf '%s\n' "${RUNTIME_PACKAGES[@]}" > "$BUNDLE/debs/PACKAGES"
	printf 'Downloaded %s package files\n' "$(ls "$BUNDLE"/debs/*.deb | wc -l)"
}

fetch_cloudflared() {
	step "Downloading cloudflared"
	local tmp
	tmp="$(mktemp -d)"
	curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg | gpg --dearmor --batch --yes -o "$tmp/cloudflare-main.gpg"
	install -m 644 "$tmp/cloudflare-main.gpg" /usr/share/keyrings/cloudflare-main.gpg
	printf 'deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main\n' > /etc/apt/sources.list.d/cloudflared.list
	apt-get update
	(cd "$tmp" && apt-get download cloudflared)
	mv "$tmp"/cloudflared_*.deb "$BUNDLE/cloudflared.deb"
	rm -rf "$tmp"
}

build_backend() {
	step "Building the backend (release)"
	(cd "$ROOT/backend" && cargo build --release)
	install -m 755 "$ROOT/backend/target/release/swap-backend" "$BUNDLE/swap-backend"
}

build_frontend() {
	step "Building the frontend"
	(cd "$ROOT/frontend" && pnpm install --frozen-lockfile && pnpm build)
	rm -rf "$BUNDLE/frontend"
	install -d "$BUNDLE/frontend"
	cp -a "$ROOT/frontend/build" "$ROOT/frontend/package.json" "$ROOT/frontend/node_modules" "$BUNDLE/frontend/"
}

build_llama_server() {
	step "Building llama-server from llama.cpp $LLAMA_CPP_TAG"
	local src
	src="$(mktemp -d)"
	git clone --quiet --depth 1 --branch "$LLAMA_CPP_TAG" https://github.com/ggml-org/llama.cpp "$src/llama.cpp"
	local head
	head="$(git -C "$src/llama.cpp" rev-parse HEAD)"
	[[ "$head" == "$LLAMA_CPP_COMMIT" ]] || die "llama.cpp tag $LLAMA_CPP_TAG resolved to $head, expected $LLAMA_CPP_COMMIT"
	# Static ggml/llama libraries and no curl, so the result is one binary that only needs libc,
	# libstdc++, and libgomp. CPU only: the server has no GPU.
	cmake -S "$src/llama.cpp" -B "$src/build" -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF \
		-DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_SERVER=ON \
		-DGGML_NATIVE=OFF >/dev/null
	cmake --build "$src/build" --config Release --target llama-server -j "$(nproc)" >/dev/null
	install -m 755 "$src/build/bin/llama-server" "$BUNDLE/llama-server"
	rm -rf "$src"
	# libgomp is not in the minimal image; add it to the package set so the debs cover it.
	(cd "$BUNDLE/debs" && apt-get download libgomp1 >/dev/null 2>&1 || true)
}

fetch_model() {
	step "Model: $MODEL_FILE"
	install -d "$BUNDLE/model"
	local path="$BUNDLE/model/$MODEL_FILE"
	if [[ ! -f "$path" ]]; then
		if ((SKIP_MODEL)); then
			printf 'Skipped. Put %s in bundle/model/ before copying, or the assistant is installed without a model.\n' "$MODEL_FILE"
			return
		fi
		printf 'Downloading %s (%s bytes)...\n' "$MODEL_FILE" "$MODEL_SIZE"
		curl -fL --retry 3 -o "$path.part" "$MODEL_URL"
		mv "$path.part" "$path"
	fi
	local actual
	actual="$(sha256sum "$path" | awk '{print $1}')"
	[[ "$actual" == "$MODEL_SHA256" ]] || die "model file hash mismatch: got $actual. Delete bundle/model/$MODEL_FILE and run again."
	cp "$ROOT/assistant/MODEL-LICENSE.md" "$BUNDLE/model/"
	printf 'Model verified.\n'
}

write_manifest() {
	step "Writing MANIFEST"
	# shellcheck disable=SC1091
	. /etc/os-release
	{
		printf 'bundle_format=1\n'
		printf 'ubuntu=%s\n' "$VERSION_ID"
		printf 'arch=%s\n' "$DEB_ARCH"
		printf 'built=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
		printf 'source_commit=%s\n' "$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
		printf 'assistant=%s\n' "$(( SKIP_ASSISTANT ? 0 : 1 ))"
	} > "$BUNDLE/BUNDLE_INFO"
	(cd "$BUNDLE" && find . -type f ! -name MANIFEST -print0 | sort -z | xargs -0 sha256sum > MANIFEST)
	printf '%s files, %s\n' "$(wc -l < "$BUNDLE/MANIFEST")" "$(du -sh "$BUNDLE" | cut -f1)"
}

main() {
	require_build_host
	# shellcheck disable=SC1091
	. "$ROOT/assistant/MODEL.conf"
	install -d "$BUNDLE"
	install_build_tools
	[[ -f "$BUNDLE/node.tar.xz" ]] || fetch_node
	fetch_debs
	fetch_cloudflared
	build_backend
	build_frontend
	if ((SKIP_ASSISTANT)); then
		rm -f "$BUNDLE/llama-server"
		rm -rf "$BUNDLE/model"
	else
		build_llama_server
		fetch_model
	fi
	write_manifest
	printf '\nBundle ready in %s. Copy the repository with this directory to the server and run install.sh there.\n' "$BUNDLE"
}

main "$@"
