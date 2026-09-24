// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later

package main

import (
	"bytes"
	"compress/gzip"
	"fmt"
	"io"
	"io/fs"
	"path"
	"strings"

	"git.foxden.network/FoxDen/NetCmdr/bridge/web"
)

// File is one client file in each encoding the bridge can serve.
type File struct {
	Plain []byte
	// Gzip is nil if gzip didn't help.
	Gzip []byte
	Type string
}

// Assets is the embedded web client.
type Assets struct {
	files map[string]*File
	// Bytes in memory, for logging.
	Size int
}

func (a *Assets) Empty() bool { return len(a.files) == 0 }

func (a *Assets) Get(key string) (*File, bool) {
	f, ok := a.files[key]
	return f, ok
}

// LoadAssets unpacks the embedded client. Files are stored gzipped (or raw,
// unsuffixed, if gzip didn't help); startup gunzips an identity copy.
func LoadAssets() (*Assets, error) {
	assets := &Assets{files: make(map[string]*File)}

	err := fs.WalkDir(web.Dist, "dist", func(name string, entry fs.DirEntry, err error) error {
		if err != nil || entry.IsDir() {
			return err
		}
		key := strings.TrimPrefix(name, "dist/")
		// .gitkeep etc.
		if strings.HasPrefix(path.Base(key), ".") {
			return nil
		}

		body, err := web.Dist.ReadFile(name)
		if err != nil {
			return err
		}

		file := &File{Plain: body}
		if strings.HasSuffix(key, ".gz") {
			key = strings.TrimSuffix(key, ".gz")
			plain, err := gunzip(body)
			if err != nil {
				// A corrupt binary; fail loudly rather than serve it.
				return fmt.Errorf("embedded %s will not decompress: %w", key, err)
			}
			file = &File{Plain: plain, Gzip: body}
		}
		file.Type = contentType(key)

		assets.files[key] = file
		assets.Size += len(file.Plain) + len(file.Gzip)
		return nil
	})
	if err != nil {
		return nil, err
	}
	return assets, nil
}

func gunzip(body []byte) ([]byte, error) {
	r, err := gzip.NewReader(bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	// io.ReadAll reports corruption; the close error doesn't matter.
	defer func() { _ = r.Close() }()
	return io.ReadAll(r)
}

// contentType is the type of a file by extension. application/wasm is
// required for streaming compilation.
func contentType(key string) string {
	switch path.Ext(key) {
	case ".html":
		return "text/html; charset=utf-8"
	case ".js", ".mjs":
		return "text/javascript; charset=utf-8"
	case ".wasm":
		return "application/wasm"
	case ".css":
		return "text/css; charset=utf-8"
	case ".json":
		return "application/json"
	case ".svg":
		return "image/svg+xml"
	case ".png":
		return "image/png"
	case ".ico":
		return "image/vnd.microsoft.icon"
	case ".woff2":
		return "font/woff2"
	}
	return "application/octet-stream"
}

// acceptsGzip reports whether an Accept-Encoding header offers gzip (q=0
// refuses it).
func acceptsGzip(header string) bool {
	for _, part := range strings.Split(header, ",") {
		fields := strings.Split(part, ";")
		if !strings.EqualFold(strings.TrimSpace(fields[0]), "gzip") {
			continue
		}
		for _, p := range fields[1:] {
			switch strings.ReplaceAll(strings.TrimSpace(p), " ", "") {
			case "q=0", "q=0.0", "q=0.00", "q=0.000":
				return false
			}
		}
		return true
	}
	return false
}
