// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later

package main

import (
	"encoding/json"
	"errors"
	"io"
	"log/slog"
	"net/http"
	"strings"
)

// Bridge serves the browser client and relays the device's protocol.
//
// | Route              | Purpose                                            |
// |--------------------|----------------------------------------------------|
// | POST /message.pv   | forwarded verbatim to the device's management API  |
// | GET /kvm?type=…    | a WebSocket carrying one session socket            |
// | GET /api/info      | what device this bridge is pointed at              |
// | anything else      | a file of the web client, which is in this binary   |
type Bridge struct {
	Device *Device
	Assets *Assets
	Log    *slog.Logger
}

// Handler wires up the routes. Methods are checked inside handlers, since a
// method pattern would let wrong methods fall through to a 404 instead of 405.
func (b *Bridge) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/message.pv", only(b.forwardMessagePV, http.MethodPost))
	mux.HandleFunc("/kvm", only(b.openChannel, http.MethodGet))
	mux.HandleFunc("/api/info", only(b.info, http.MethodGet))
	mux.HandleFunc("/", only(b.serveStatic, http.MethodGet, http.MethodHead))
	return refuseTraversal(mux)
}

// only answers other methods with a 405.
func only(next http.HandlerFunc, methods ...string) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		for _, method := range methods {
			if r.Method == method {
				next(w, r)
				return
			}
		}
		w.Header().Set("Allow", strings.Join(methods, ", "))
		http.Error(w, "Method Not Allowed", http.StatusMethodNotAllowed)
	}
}

// refuseTraversal rejects ".." paths before ServeMux turns them into
// redirects.
func refuseTraversal(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		for _, segment := range strings.Split(r.URL.Path, "/") {
			if segment == ".." {
				http.Error(w, "Forbidden", http.StatusForbidden)
				return
			}
		}
		next.ServeHTTP(w, r)
	})
}

func (b *Bridge) info(w http.ResponseWriter, r *http.Request) {
	body, err := json.Marshal(map[string]any{
		"device":      b.Device.Host,
		"sessionPort": b.Device.SessionPort,
	})
	if err != nil {
		http.Error(w, "Internal Server Error", http.StatusInternalServerError)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Cache-Control", "no-store")
	// The client is gone; nothing to do.
	_, _ = w.Write(body)
}

// forwardMessagePV forwards an opaque management request and its reply.
func (b *Bridge) forwardMessagePV(w http.ResponseWriter, r *http.Request) {
	body, err := readLimited(r)
	if err != nil {
		http.Error(w, "Bad Request", http.StatusBadRequest)
		return
	}

	reply, err := b.Device.PostMessagePV(string(body))
	if err != nil {
		b.Log.Warn("management request failed", "error", err)
		http.Error(w, "device: "+err.Error(), http.StatusBadGateway)
		return
	}

	w.Header().Set("Content-Type", "text/xml; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	_, _ = w.Write([]byte(reply))
}

// serveStatic serves an embedded client file ("/" is index.html), or 404.
func (b *Bridge) serveStatic(w http.ResponseWriter, r *http.Request) {
	key := strings.TrimPrefix(r.URL.Path, "/")
	if key == "" {
		key = "index.html"
	}

	file, ok := b.Assets.Get(key)
	if !ok {
		http.Error(w, "Not Found", http.StatusNotFound)
		return
	}

	w.Header().Set("Content-Type", file.Type)
	w.Header().Set("Cache-Control", "no-store")

	body := file.Plain
	if file.Gzip != nil && acceptsGzip(r.Header.Get("Accept-Encoding")) {
		w.Header().Set("Content-Encoding", "gzip")
		w.Header().Set("Vary", "Accept-Encoding")
		body = file.Gzip
	}
	_, _ = w.Write(body)
}

// maxBody caps a request body; large enough for a base64 firmware upload.
const maxBody = 64 * 1024 * 1024

func readLimited(r *http.Request) ([]byte, error) {
	defer func() { _ = r.Body.Close() }()
	body, err := io.ReadAll(io.LimitReader(r.Body, maxBody+1))
	if err != nil {
		return nil, err
	}
	if len(body) > maxBody {
		return nil, errors.New("request body too large")
	}
	return body, nil
}
