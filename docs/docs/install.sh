#!/bin/sh
# Installs the latest smelt release for Linux or macOS.
#
#   curl -fsSL https://smeltagent.dev/install.sh | sh
#
# Environment:
#   SMELT_VERSION      release tag to install, e.g. v0.6.0 (default: latest)
#   SMELT_INSTALL_DIR  where to put the binary (default: ~/.local/bin)

set -eu

repo="leonardcser/smelt"
install_dir="${SMELT_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf 'smelt: %s\n' "$*"; }
die() { printf 'smelt: error: %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required"; }

fetch() {
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --retry 3 -o "$2" "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -q -O "$2" "$1"
	else
		die "curl or wget is required"
	fi
}

sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d' ' -f1
	else
		die "sha256sum or shasum is required"
	fi
}

detect_target() {
	os="$(uname -s)"
	arch="$(uname -m)"

	case "$arch" in
	x86_64 | amd64) arch="x86_64" ;;
	aarch64 | arm64) arch="aarch64" ;;
	*) die "unsupported architecture: $arch" ;;
	esac

	case "$os" in
	Linux)
		if ldd --version 2>&1 | grep -qi musl; then
			die "musl libc is not supported; install from source with cargo"
		fi
		echo "$arch-unknown-linux-gnu"
		;;
	Darwin)
		# An x86_64 shell under Rosetta still runs the native arm64 build.
		if [ "$arch" = "x86_64" ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = "1" ]; then
			arch="aarch64"
		fi
		echo "$arch-apple-darwin"
		;;
	MINGW* | MSYS* | CYGWIN*)
		die "on Windows, download smelt-x86_64-pc-windows-msvc.tar.gz from https://github.com/$repo/releases"
		;;
	*) die "unsupported OS: $os" ;;
	esac
}

main() {
	need tar
	need uname

	target="$(detect_target)"
	archive="smelt-$target.tar.gz"
	if [ -n "${SMELT_VERSION:-}" ]; then
		base="https://github.com/$repo/releases/download/$SMELT_VERSION"
	else
		base="https://github.com/$repo/releases/latest/download"
	fi

	tmp="$(mktemp -d)"
	trap 'rm -rf "$tmp"' EXIT INT TERM

	say "downloading $archive"
	fetch "$base/$archive" "$tmp/$archive" || die "download failed: $base/$archive"
	fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"

	expected="$(grep " $archive\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
	[ -n "$expected" ] || die "$archive is missing from SHA256SUMS"
	[ "$(sha256 "$tmp/$archive")" = "$expected" ] || die "checksum mismatch for $archive"

	tar -xzf "$tmp/$archive" -C "$tmp" smelt
	mkdir -p "$install_dir"
	mv "$tmp/smelt" "$install_dir/smelt"
	chmod 755 "$install_dir/smelt"

	say "installed $("$install_dir/smelt" --version 2>/dev/null || echo smelt) to $install_dir/smelt"

	case ":$PATH:" in
	*":$install_dir:"*) say "run 'smelt' to get started" ;;
	*)
		say "$install_dir is not on your PATH; add this to your shell profile:"
		printf '\n  export PATH="%s:$PATH"\n\n' "$install_dir"
		;;
	esac
}

main "$@"
