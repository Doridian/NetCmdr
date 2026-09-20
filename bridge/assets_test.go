// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later

package main

import (
	"strings"
	"testing"
)

func TestAnAcceptEncodingHeaderIsReadPerToken(t *testing.T) {
	for header, want := range map[string]bool{
		"gzip, deflate, br":    true,
		"gzip;q=1.0, br;q=0.8": true,
		"br":                   false,
		"":                     false,
		// Naming an encoding at q=0 is how a client refuses it specifically.
		"gzip;q=0, br": false,
		// A prefix is not a match.
		"gzipper": false,
		"GZIP":    true,
	} {
		if got := acceptsGzip(header); got != want {
			t.Errorf("acceptsGzip(%q) = %v; want %v", header, got, want)
		}
	}
}

func TestWasmGetsTheTypeBrowsersInsistOn(t *testing.T) {
	if got := contentType("netcmdr_web_bg.wasm"); got != "application/wasm" {
		t.Fatalf("got %q; want application/wasm", got)
	}
}

func TestContentTypeByExtension(t *testing.T) {
	for key, want := range map[string]string{
		"index.html":    "text/html; charset=utf-8",
		"app.js":        "text/javascript; charset=utf-8",
		"a/b/style.css": "text/css; charset=utf-8",
		"favicon.ico":   "image/vnd.microsoft.icon",
		"unknown.bin":   "application/octet-stream",
		"noextension":   "application/octet-stream",
	} {
		if got := contentType(key); got != want {
			t.Errorf("contentType(%q) = %q; want %q", key, got, want)
		}
	}
}

// Whatever the packer wrote has to come back out of the binary intact, under
// the URL path the client asks for rather than the suffix it was stored with.
func TestTheEmbeddedClientUnpacks(t *testing.T) {
	assets, err := LoadAssets()
	if err != nil {
		t.Fatal(err)
	}
	if assets.Empty() {
		t.Skip("no web client packed into this build; run tools/pack-web.sh")
	}

	for key, file := range assets.files {
		if strings.HasSuffix(key, ".gz") {
			t.Errorf("%q kept its storage suffix; the URL path must not carry it", key)
		}
		if len(file.Plain) == 0 {
			t.Errorf("%q unpacked to nothing", key)
		}
		if file.Gzip != nil && len(file.Gzip) >= len(file.Plain) {
			t.Errorf("%q kept a gzip copy that is not smaller", key)
		}
		if file.Type == "" {
			t.Errorf("%q has no content type", key)
		}
	}
	if _, ok := assets.Get("index.html"); !ok {
		t.Error("the client has no index.html to serve at /")
	}
}
