// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later

package main

import (
	"context"
	"errors"
	"io"
	"net"
	"net/http"
	"time"
	"unicode/utf8"

	"github.com/coder/websocket"
)

// readLimit caps one browser-to-device message.
//
// Input messages are 28 bytes, so this is never reached in practice; it exists
// so a browser cannot make us buffer without limit.
const readLimit = 1024 * 1024

// openChannel opens one session socket and relays it over a WebSocket.
//
// The upgrade happens *before* the device is contacted, because the session id
// arrives on the WebSocket rather than in the URL — see readSessionID for why.
// That also means a device refusal comes back as a close frame with a reason,
// which is the only form a browser will show the user.
func (b *Bridge) openChannel(w http.ResponseWriter, r *http.Request) {
	socket, ok := socketTypeByName(r.URL.Query().Get("type"))
	if !ok {
		http.Error(w, "Bad Request", http.StatusBadRequest)
		return
	}

	// The default origin check is same-origin, which is what this is: the page
	// holding the client was served by this same bridge. It is the one thing
	// standing between a device on loopback and any web page the operator
	// happens to open, since the bridge authenticates nobody.
	conn, err := websocket.Accept(w, r, &websocket.AcceptOptions{
		// The payload is the device's own video codec and input records —
		// already compressed, or too small to be worth a deflate context per
		// connection.
		CompressionMode: websocket.CompressionDisabled,
	})
	if err != nil {
		b.Log.Debug("upgrade refused", "error", err)
		return
	}
	// CloseNow is the backstop for the paths that do not close politely below;
	// on the ones that do it is a second close, which reports an error and has
	// nothing left to do.
	defer func() { _ = conn.CloseNow() }()
	conn.SetReadLimit(readLimit)

	ctx := r.Context()
	sessionID, err := readSessionID(ctx, conn, b.Device.Timeout)
	if err != nil {
		b.Log.Debug("no session id", "error", err)
		_ = conn.Close(websocket.StatusPolicyViolation, "expected the session id first")
		return
	}

	device, err := b.Device.DialSession(sessionID, socket)
	if err != nil {
		b.Log.Warn("device refused the channel", "type", socket, "error", err)
		_ = conn.Close(websocket.StatusInternalError, truncateReason("device: "+err.Error()))
		return
	}
	defer func() { _ = device.Close() }()

	b.Log.Info("channel relaying", "type", socket)
	if err := relay(ctx, conn, device); err != nil {
		b.Log.Warn("channel failed", "type", socket, "error", err)
		return
	}
	b.Log.Info("channel closed", "type", socket)
}

// readSessionID reads the session id the client sends as its first message.
//
// # Why this is not in the URL
//
// The device's SESSIONID is base64 — measured on firmware 2.2.1258.1.0, a
// 44-character token containing '+', '/' and '=', which are exactly the
// characters a query string handles badly. That alone would only call for
// careful encoding, but two other things settle it:
//
//   - It is a credential, and credentials in URLs end up in access logs, proxy
//     logs and browser history in a way message payloads do not.
//   - A browser never exposes the body of a failed WebSocket handshake. If the
//     device refuses, an HTTP status here would reach the page as nothing more
//     than "connection failed". Upgrading first and then refusing with a close
//     frame is the only way the reason reaches the client at all.
//
// So the upgrade always succeeds, the first frame carries the token as text,
// and everything after it is opaque protocol data.
func readSessionID(ctx context.Context, conn *websocket.Conn, timeout time.Duration) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()

	kind, data, err := conn.Read(ctx)
	if err != nil {
		return "", err
	}
	if kind != websocket.MessageText {
		return "", errors.New("the first message must be the session id, as text")
	}
	return string(data), nil
}

// relay moves bytes between the browser and the device until either side stops.
//
// Binary frames, each an opaque slice of the device's byte stream. The bridge
// deliberately does *not* understand KVMMsg framing: message boundaries on the
// WebSocket have nothing to do with message boundaries in the protocol, and the
// client reassembles the stream exactly as it would from a socket.
//
// Both directions run as plain copies on their own goroutine, which a
// *tls.Conn supports directly — it holds separate locks for reading and
// writing. Whichever direction ends first closes both sides, which unblocks the
// other copy.
func relay(ctx context.Context, conn *websocket.Conn, device net.Conn) error {
	browser := websocket.NetConn(ctx, conn, websocket.MessageBinary)

	done := make(chan error, 2)
	go func() { _, err := io.Copy(device, browser); done <- err }()
	go func() { _, err := io.Copy(browser, device); done <- err }()

	err := <-done
	// Whoever lost the race is still parked in a read; closing both ends it.
	_ = device.Close()
	_ = browser.Close()
	<-done

	if err == nil || errors.Is(err, io.EOF) || errors.Is(err, net.ErrClosed) {
		return nil
	}
	// A browser that simply goes away closes the socket; that is how a session
	// normally ends, not a failure.
	if websocket.CloseStatus(err) != -1 || errors.Is(err, context.Canceled) {
		return nil
	}
	return err
}

// truncateReason keeps a close reason inside the 123 bytes a control frame's
// payload allows, so that a long device error still reaches the browser rather
// than failing to send at all.
func truncateReason(reason string) string {
	const max = 123
	if len(reason) <= max {
		return reason
	}
	// Cut on a rune boundary: a close frame's payload must be valid UTF-8.
	cut := max
	for cut > 0 && !utf8.RuneStart(reason[cut]) {
		cut--
	}
	return reason[:cut]
}
