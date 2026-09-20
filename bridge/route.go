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

// Handler wires the routes up. Everything not claimed by one of the three
// protocol routes is a file of the client.
//
// The methods are checked inside each route rather than declared in the
// pattern, because the catch-all below matches every path: a pattern of
// "POST /message.pv" would leave a PUT to that path falling through to the
// static handler and coming back as a 404, where the wrong method on a route
// that exists is a 405.
func (b *Bridge) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/message.pv", only(b.forwardMessagePV, http.MethodPost))
	mux.HandleFunc("/kvm", only(b.openChannel, http.MethodGet))
	mux.HandleFunc("/api/info", only(b.info, http.MethodGet))
	mux.HandleFunc("/", only(b.serveStatic, http.MethodGet, http.MethodHead))
	return refuseTraversal(mux)
}

// only answers anything but the named methods with a 405.
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

// refuseTraversal turns away a path that tries to climb out of the client.
//
// It has to run before ServeMux, which would otherwise normalise such a path
// and answer with a redirect to wherever it pointed. There is no filesystem
// behind this, so the path could at worst miss the table; refusing it keeps the
// answer to a traversal attempt an unambiguous no rather than a redirect or a
// 404 that looks like a typo.
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
	// A write that fails here is a browser that has gone away mid-response:
	// the status is already sent, so there is no second answer to give it.
	_, _ = w.Write(body)
}

// forwardMessagePV hands a management request to the device and the answer
// back.
//
// The body is opaque here — it is the device's own XML, and the bridge has no
// reason to look inside it. That keeps every management call the web client
// makes working without the bridge knowing any of them exist.
func (b *Bridge) forwardMessagePV(w http.ResponseWriter, r *http.Request) {
	body, err := readLimited(r)
	if err != nil {
		http.Error(w, "Bad Request", http.StatusBadRequest)
		return
	}

	reply, err := b.Device.PostMessagePV(string(body))
	if err != nil {
		b.Log.Warn("management request failed", "error", err)
		// The device is upstream of us, so its failures are 502s.
		http.Error(w, "device: "+err.Error(), http.StatusBadGateway)
		return
	}

	w.Header().Set("Content-Type", "text/xml; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	_, _ = w.Write([]byte(reply))
}

// serveStatic serves a file of the embedded web client, or 404.
//
// "/" means index.html. Nothing is served that was not built in — there is no
// directory behind this to grow a file in. A path that tries to climb out was
// already refused by refuseTraversal above.
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

	// The type is the original file's; the encoding is separate from it.
	w.Header().Set("Content-Type", file.Type)
	w.Header().Set("Cache-Control", "no-store")

	body := file.Plain
	if file.Gzip != nil && acceptsGzip(r.Header.Get("Accept-Encoding")) {
		// Vary, or a cache in front would hand a gzip body to a client that
		// never asked for one.
		w.Header().Set("Content-Encoding", "gzip")
		w.Header().Set("Vary", "Accept-Encoding")
		body = file.Gzip
	}
	_, _ = w.Write(body)
}

// maxBody caps a request body.
//
// Ordinary /message.pv calls are tiny, but a firmware upload is the whole image
// base64-encoded inside the XML, so the ceiling has to clear that with room to
// spare rather than sit at a "sensible" few hundred kilobytes.
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
