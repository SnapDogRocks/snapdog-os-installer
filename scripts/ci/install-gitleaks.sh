#!/usr/bin/env bash
# Install the pinned gitleaks into a directory: install-gitleaks.sh <destination>.
#
# The first source is the release binary on GitHub, checked against its pinned SHA-256. When GitHub
# does not deliver it (release downloads answered 504 for hours on 2026-11-08), the same tag is
# built from the Go module proxy. That source does not depend on GitHub's release storage, and go
# verifies the module against the checksum database sum.golang.org, so the fallback is not a weaker
# download. A release archive that does not match its checksum is not a download failure: it stops
# the job instead of falling back.
set -euo pipefail

version=8.30.1
digest=551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb
module=github.com/zricethezav/gitleaks/v8
release_url=${GITLEAKS_RELEASE_URL:-https://github.com/gitleaks/gitleaks/releases/download/v${version}/gitleaks_${version}_linux_x64.tar.gz}

destination=${1:?destination directory required}
mkdir -p "$destination"
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT

if curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    --retry 3 --retry-all-errors --retry-delay 5 --max-time 120 \
    "$release_url" --output "$temporary/gitleaks.tar.gz"; then
    actual=$(sha256sum "$temporary/gitleaks.tar.gz" | cut -d ' ' -f 1)
    [[ $actual == "$digest" ]] || {
        echo "gitleaks ${version}: the release archive has SHA-256 ${actual}, not the pinned ${digest}" >&2
        exit 1
    }
    tar -xzf "$temporary/gitleaks.tar.gz" -C "$destination" gitleaks
    echo "gitleaks ${version}: installed the GitHub release binary"
else
    echo "::warning::gitleaks ${version}: the GitHub release download failed, building it from proxy.golang.org"
    go=$(command -v go || true)
    if [[ -z $go ]]; then
        # GitHub's runner images keep Go in the tool cache even when it is not on PATH.
        go=$(find /opt/hostedtoolcache/go -path '*/x64/bin/go' 2>/dev/null | sort -V | tail -n 1)
    fi
    [[ -n $go ]] || {
        echo "gitleaks ${version}: no go toolchain for the fallback" >&2
        exit 1
    }
    (
        cd "$temporary"
        GOBIN="$destination" GOPROXY=https://proxy.golang.org GOSUMDB=sum.golang.org \
            GONOSUMDB='' GOPRIVATE='' GOINSECURE='' GOFLAGS='' GOTOOLCHAIN=auto \
            "$go" install "${module}@v${version}"
    )
    echo "gitleaks ${version}: built from the Go module proxy"
fi
"$destination/gitleaks" version >/dev/null
