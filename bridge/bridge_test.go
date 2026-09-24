// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later

// End-to-end relay tests over the (unencrypted) video channel. TLS is covered
// in device_test.go.

package main

import (
	"bufio"
	"bytes"
	"compress/gzip"
	"context"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
)

func testAssets(t *testing.T, files map[string][]byte) *Assets {
	t.Helper()
	assets := &Assets{files: make(map[string]*File)}
	for key, body := range files {
		var buf bytes.Buffer
		zw, _ := gzip.NewWriterLevel(&buf, gzip.BestCompression)
		if _, err := zw.Write(body); err != nil {
			t.Fatal(err)
		}
		if err := zw.Close(); err != nil {
			t.Fatal(err)
		}

		file := &File{Plain: body, Type: contentType(key)}
		if buf.Len() < len(body) {
			file.Gzip = buf.Bytes()
		}
		assets.files[key] = file
	}
	return assets
}

func testBridge(t *testing.T, sessionPort uint16, files map[string][]byte) *httptest.Server {
	t.Helper()
	bridge := &Bridge{
		Device: &Device{
			Host:        "127.0.0.1",
			HTTPSPort:   443,
			SessionPort: sessionPort,
			Timeout:     5 * time.Second,
		},
		Assets: testAssets(t, files),
		Log:    slog.New(slog.DiscardHandler),
	}
	server := httptest.NewServer(bridge.Handler())
	t.Cleanup(server.Close)
	return server
}

// dialKVM opens the relay and sends the session id.
func dialKVM(t *testing.T, server *httptest.Server, kind, session string) *websocket.Conn {
	t.Helper()
	url := "ws" + strings.TrimPrefix(server.URL, "http") + "/kvm?type=" + kind
	conn, _, err := websocket.Dial(context.Background(), url, nil)
	if err != nil {
		t.Fatalf("upgrade refused: %v", err)
	}
	t.Cleanup(func() { _ = conn.CloseNow() })

	if err := conn.Write(context.Background(), websocket.MessageText, []byte(session)); err != nil {
		t.Fatal(err)
	}
	return conn
}

func TestCarriesBytesFromTheBrowserToTheDevice(t *testing.T) {
	got := make(chan string, 1)
	port := fakeDevice(t, SocketVideo, nil, func(_ net.Conn, br *bufio.Reader) {
		buf := make([]byte, 12)
		if _, err := io.ReadFull(br, buf); err != nil {
			got <- "read failed: " + err.Error()
			return
		}
		got <- string(buf)
	})

	conn := dialKVM(t, testBridge(t, port, nil), "video", "testsession")
	if err := conn.Write(context.Background(), websocket.MessageBinary, []byte("hello device")); err != nil {
		t.Fatal(err)
	}

	select {
	case text := <-got:
		if text != "hello device" {
			t.Fatalf("device received %q; want \"hello device\"", text)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("the device never received the browser's bytes")
	}
}

func TestCarriesBytesFromTheDeviceToTheBrowser(t *testing.T) {
	port := fakeDevice(t, SocketVideo, nil, func(conn net.Conn, _ *bufio.Reader) {
		_, _ = conn.Write([]byte("hello browser"))
		// Keep open.
		time.Sleep(2 * time.Second)
	})

	conn := dialKVM(t, testBridge(t, port, nil), "video", "testsession")

	// The relay may split or coalesce; reassemble until it is all here.
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	var got []byte
	for len(got) < 13 {
		kind, data, err := conn.Read(ctx)
		if err != nil {
			t.Fatalf("after %d of 13 bytes: %v", len(got), err)
		}
		if kind != websocket.MessageBinary {
			t.Fatalf("got a %v frame; the relay carries binary", kind)
		}
		got = append(got, data...)
	}
	if string(got) != "hello browser" {
		t.Fatalf("browser received %q; want \"hello browser\"", got)
	}
}

// The device hanging up has to reach the browser as a close, not a hang.
func TestClosesTheBrowserSideWhenTheDeviceGoesAway(t *testing.T) {
	port := fakeDevice(t, SocketVideo, nil, func(conn net.Conn, _ *bufio.Reader) {
		_ = conn.Close()
	})

	conn := dialKVM(t, testBridge(t, port, nil), "video", "testsession")
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	for {
		if _, _, err := conn.Read(ctx); err != nil {
			if ctx.Err() != nil {
				t.Fatal("the browser was never told the device left")
			}
			return
		}
	}
}

// A device refusal must arrive as a close frame with a reason.
func TestADeviceRefusalReachesTheBrowserAsACloseReason(t *testing.T) {
	// Nothing is listening on this port, so the dial fails.
	free, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := uint16(free.Addr().(*net.TCPAddr).Port)
	if err := free.Close(); err != nil {
		t.Fatal(err)
	}

	conn := dialKVM(t, testBridge(t, port, nil), "video", "testsession")
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	_, _, err = conn.Read(ctx)
	if err == nil {
		t.Fatal("a refused channel must close the WebSocket")
	}
	if status := websocket.CloseStatus(err); status == -1 {
		t.Fatalf("want a close frame carrying a reason, got %v", err)
	}
	if !strings.Contains(err.Error(), "device:") {
		t.Fatalf("the close reason must name the device: %v", err)
	}
}

// Binary first means the client never said who it is.
func TestTheFirstMessageMustBeTheSessionIDAsText(t *testing.T) {
	port := fakeDevice(t, SocketVideo, nil, func(net.Conn, *bufio.Reader) {})
	server := testBridge(t, port, nil)

	url := "ws" + strings.TrimPrefix(server.URL, "http") + "/kvm?type=video"
	conn, _, err := websocket.Dial(context.Background(), url, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = conn.CloseNow() }()

	if err := conn.Write(context.Background(), websocket.MessageBinary, []byte("not an id")); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if _, _, err := conn.Read(ctx); websocket.CloseStatus(err) != websocket.StatusPolicyViolation {
		t.Fatalf("want a policy-violation close, got %v", err)
	}
}

func TestServesTheClientAndItsStatuses(t *testing.T) {
	compressible := []byte(strings.Repeat("const module = 'plain-bytes';\n", 64))
	server := testBridge(t, 900, map[string][]byte{
		"index.html":    []byte("<!doctype html>"),
		"app.wasm":      compressible,
		"assets/app.js": compressible,
	})
	client := server.Client()
	client.CheckRedirect = func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }

	get := func(t *testing.T, method, path, acceptEncoding string) *http.Response {
		t.Helper()
		req, err := http.NewRequest(method, server.URL+path, nil)
		if err != nil {
			t.Fatal(err)
		}
		// Disable transparent decompression.
		req.Header.Set("Accept-Encoding", acceptEncoding)
		resp, err := client.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { _ = resp.Body.Close() })
		return resp
	}

	t.Run("the root is the index", func(t *testing.T) {
		resp := get(t, "GET", "/", "identity")
		body, _ := io.ReadAll(resp.Body)
		if resp.StatusCode != 200 || string(body) != "<!doctype html>" {
			t.Fatalf("got %d %q", resp.StatusCode, body)
		}
		if ct := resp.Header.Get("Content-Type"); !strings.HasPrefix(ct, "text/html") {
			t.Fatalf("content type was %q", ct)
		}
	})

	t.Run("a gzip copy is served when the client takes it", func(t *testing.T) {
		resp := get(t, "GET", "/app.wasm", "gzip")
		if resp.Header.Get("Content-Encoding") != "gzip" {
			t.Fatal("want the gzip copy")
		}
		if resp.Header.Get("Vary") != "Accept-Encoding" {
			t.Fatal("a compressed answer must Vary, or a cache in front misfeeds it")
		}
		// Browsers refuse to stream-compile a module served as anything else.
		if ct := resp.Header.Get("Content-Type"); ct != "application/wasm" {
			t.Fatalf("content type was %q; want application/wasm", ct)
		}
		zr, err := gzip.NewReader(resp.Body)
		if err != nil {
			t.Fatal(err)
		}
		got, _ := io.ReadAll(zr)
		if !bytes.Equal(got, compressible) {
			t.Fatal("the gzip copy does not unpack to the file")
		}
	})

	t.Run("a client that takes no coding gets the file itself", func(t *testing.T) {
		resp := get(t, "GET", "/app.wasm", "identity")
		if resp.Header.Get("Content-Encoding") != "" {
			t.Fatal("want the plain copy")
		}
		got, _ := io.ReadAll(resp.Body)
		if !bytes.Equal(got, compressible) {
			t.Fatal("the plain copy is not the file")
		}
	})

	t.Run("nested paths are served", func(t *testing.T) {
		resp := get(t, "GET", "/assets/app.js", "gzip")
		if resp.StatusCode != 200 {
			t.Fatalf("got %d", resp.StatusCode)
		}
		if ct := resp.Header.Get("Content-Type"); !strings.HasPrefix(ct, "text/javascript") {
			t.Fatalf("content type was %q", ct)
		}
	})

	t.Run("nothing that was not built in is served", func(t *testing.T) {
		if resp := get(t, "GET", "/netcmdr_web.js", "gzip"); resp.StatusCode != 404 {
			t.Fatalf("got %d; want 404", resp.StatusCode)
		}
	})

	t.Run("traversal is refused rather than normalised", func(t *testing.T) {
		if resp := get(t, "GET", "/../etc/passwd", "identity"); resp.StatusCode != 403 {
			t.Fatalf("got %d; want 403", resp.StatusCode)
		}
	})

	t.Run("the wrong method on a route that exists is a 405", func(t *testing.T) {
		for _, path := range []string{"/message.pv", "/kvm", "/api/info", "/anything"} {
			if resp := get(t, "PUT", path, "identity"); resp.StatusCode != 405 {
				t.Errorf("PUT %s gave %d; want 405", path, resp.StatusCode)
			}
		}
	})

	t.Run("api/info names the device", func(t *testing.T) {
		resp := get(t, "GET", "/api/info", "identity")
		body, _ := io.ReadAll(resp.Body)
		if !strings.Contains(string(body), `"device":"127.0.0.1"`) ||
			!strings.Contains(string(body), `"sessionPort":900`) {
			t.Fatalf("got %s", body)
		}
	})

	t.Run("an unknown channel name is refused", func(t *testing.T) {
		if resp := get(t, "GET", "/kvm?type=general", "identity"); resp.StatusCode != 400 {
			t.Fatalf("got %d; want 400", resp.StatusCode)
		}
	})
}
