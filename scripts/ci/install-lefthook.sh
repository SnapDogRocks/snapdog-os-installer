#!/usr/bin/env bash
# Install the pinned lefthook into a directory: install-lefthook.sh <destination>.
#
# The first source is the release on GitHub, checked against its pinned SHA-256. When GitHub does
# not deliver it (release downloads answered 504 for hours on 2026-11-08), the same tag is built
# from the Go module proxy. That source does not depend on GitHub's release storage, and go
# verifies the module against the checksum database sum.golang.org, so the fallback is not a weaker
# download. A release file that does not match its checksum is not a download failure: it stops
# the job instead of falling back.
set -euo pipefail

version=2.1.12
digest=22ff1ad48d1a0f4dca8d6b7e920056c6a9015f9204e5b693858ed9c2db759a16
module=github.com/evilmartians/lefthook/v2
release_url=${LEFTHOOK_RELEASE_URL:-https://github.com/evilmartians/lefthook/releases/download/v${version}/lefthook_${version}_Linux_x86_64}

destination=${1:?destination directory required}
mkdir -p "$destination"
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT

if curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    --retry 3 --retry-all-errors --retry-delay 5 --max-time 120 \
    "$release_url" --output "$temporary/lefthook"; then
    actual=$(sha256sum "$temporary/lefthook" | cut -d ' ' -f 1)
    [[ $actual == "$digest" ]] || {
        echo "lefthook ${version}: the release file has SHA-256 ${actual}, not the pinned ${digest}" >&2
        exit 1
    }
    install -m 0755 "$temporary/lefthook" "$destination/lefthook"
    echo "lefthook ${version}: installed the GitHub release binary"
else
    echo "::warning::lefthook ${version}: the GitHub release download failed, building it from proxy.golang.org"
    go=$(command -v go || true)
    if [[ -z $go ]]; then
        # GitHub's runner images keep Go in the tool cache even when it is not on PATH.
        go=$(find /opt/hostedtoolcache/go -path '*/x64/bin/go' 2>/dev/null | sort -V | tail -n 1)
    fi
    [[ -n $go ]] || {
        echo "lefthook ${version}: no go toolchain for the fallback" >&2
        exit 1
    }
    (
        cd "$temporary"
        GOBIN="$destination" GOPROXY=https://proxy.golang.org GOSUMDB=sum.golang.org \
            GONOSUMDB='' GOPRIVATE='' GOINSECURE='' GOFLAGS='' GOTOOLCHAIN=auto \
            "$go" install "${module}@v${version}"
    )
    echo "lefthook ${version}: built from the Go module proxy"
fi
"$destination/lefthook" version >/dev/null
