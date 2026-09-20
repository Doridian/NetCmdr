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

// File is one file of the client, in every coding the bridge can send it in.
type File struct {
	Plain []byte
	// Gzip is nil where gzip did not make the file smaller, so there is no
	// second copy worth the memory.
	Gzip []byte
	Type string
}

// Assets is the whole client, ready to serve.
//
// The bridge is one file: there is no directory to point it at, nothing to
// deploy beside it and nothing it can be served a stale or hostile copy of. The
// client it hands out is the one it was built with, so nothing needs
// revalidating per request and no file can change underneath it.
type Assets struct {
	files map[string]*File
	// Bytes held in memory, for the line the bridge logs.
	Size int
}

func (a *Assets) Empty() bool { return len(a.files) == 0 }

func (a *Assets) Get(key string) (*File, bool) {
	f, ok := a.files[key]
	return f, ok
}

// LoadAssets unpacks the embedded client.
//
// The stored copy is the gzip one, which is both the smaller thing to carry in
// the binary and the copy most browsers are served, so the compression happens
// once at build time and never again. What is left for startup is the cheap
// half — gunzip each file for the clients that take no coding at all, which is
// milliseconds for a client this size.
//
// A file that gzip did not shrink was packed as it is, under no suffix.
func LoadAssets() (*Assets, error) {
	assets := &Assets{files: make(map[string]*File)}

	err := fs.WalkDir(web.Dist, "dist", func(name string, entry fs.DirEntry, err error) error {
		if err != nil || entry.IsDir() {
			return err
		}
		key := strings.TrimPrefix(name, "dist/")
		// .gitkeep, and anything else the packer did not put there.
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
				// These bytes were written by this repo's own packer and read
				// from the executable's own image, so this means a corrupt
				// binary. Serving a client with a hole in it would reach the
				// browser as an unexplained failure to start, which is a worse
				// way to find out.
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
	// Nothing is read after this, so a close error has nothing left to spoil:
	// io.ReadAll below is what reports a truncated or corrupt stream.
	defer func() { _ = r.Close() }()
	return io.ReadAll(r)
}

// contentType is the type of a file by extension.
//
// application/wasm is the one that actually matters: browsers refuse to
// stream-compile a module served as anything else, and the failure reads as a
// generic instantiation error rather than a content-type complaint.
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

// acceptsGzip reports whether an Accept-Encoding header offers gzip.
//
// Naming an encoding at q=0 is how a client refuses that one specifically,
// which is the one case where naming it does not mean accepting it.
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
