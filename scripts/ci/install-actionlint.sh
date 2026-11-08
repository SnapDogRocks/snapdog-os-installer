#!/usr/bin/env bash
# Install the pinned actionlint into a directory: install-actionlint.sh <destination>.
#
# The first source is the release on GitHub, checked against its pinned SHA-256. When GitHub does
# not deliver it (release downloads answered 504 for hours on 2026-11-08), the same tag is built
# from the Go module proxy. That source does not depend on GitHub's release storage, and go
# verifies the module against the checksum database sum.golang.org, so the fallback is not a weaker
# download. A release file that does not match its checksum is not a download failure: it stops
# the job instead of falling back.
set -euo pipefail

version=1.7.12
digest=8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8
module=github.com/rhysd/actionlint/cmd/actionlint
release_url=${ACTIONLINT_RELEASE_URL:-https://github.com/rhysd/actionlint/releases/download/v${version}/actionlint_${version}_linux_amd64.tar.gz}

destination=${1:?destination directory required}
mkdir -p "$destination"
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT

if curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
    --retry 3 --retry-all-errors --retry-delay 5 --max-time 120 \
    "$release_url" --output "$temporary/actionlint.tar.gz"; then
    actual=$(sha256sum "$temporary/actionlint.tar.gz" | cut -d ' ' -f 1)
    [[ $actual == "$digest" ]] || {
        echo "actionlint ${version}: the release file has SHA-256 ${actual}, not the pinned ${digest}" >&2
        exit 1
    }
    tar -xzf "$temporary/actionlint.tar.gz" -C "$destination" actionlint
    echo "actionlint ${version}: installed the GitHub release binary"
else
    echo "::warning::actionlint ${version}: the GitHub release download failed, building it from proxy.golang.org"
    go=$(command -v go || true)
    if [[ -z $go ]]; then
        # GitHub's runner images keep Go in the tool cache even when it is not on PATH.
        go=$(find /opt/hostedtoolcache/go -path '*/x64/bin/go' 2>/dev/null | sort -V | tail -n 1)
    fi
    [[ -n $go ]] || {
        echo "actionlint ${version}: no go toolchain for the fallback" >&2
        exit 1
    }
    (
        cd "$temporary"
        GOBIN="$destination" GOPROXY=https://proxy.golang.org GOSUMDB=sum.golang.org \
            GONOSUMDB='' GOPRIVATE='' GOINSECURE='' GOFLAGS='' GOTOOLCHAIN=auto \
            "$go" install "${module}@v${version}"
    )
    echo "actionlint ${version}: built from the Go module proxy"
fi
"$destination/actionlint" -version >/dev/null
