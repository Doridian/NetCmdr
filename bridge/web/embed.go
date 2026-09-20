// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later

// Package web carries the browser client inside the bridge binary.
//
// The directory this embeds is not in the repository: tools/pack-web.sh fills
// it from what tools/build-web.sh produced, and .gitkeep is there so that the
// embed directive below still has something to match when nobody has run
// either. A bridge built that way serves the protocol routes and no client,
// which is what keeps `go build ./...` working for someone with no wasm
// toolchain installed.
//
// Files arrive gzipped, under a .gz suffix, because the client is a WebAssembly
// module of several megabytes that the browser must have in full before
// anything appears. Compressing once at build time rather than once per start
// is why the suffix exists at all; see assets.go for what is done with it.
package web

import "embed"

// Dist is the packed client. The "all:" prefix is what makes .gitkeep match,
// so an unpacked tree is an empty client rather than a compile error.
//
//go:embed all:dist
var Dist embed.FS
