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

// readLimit caps one browser-to-device message (input messages are 28 bytes).
const readLimit = 1024 * 1024

// openChannel opens one session socket and relays it over a WebSocket. The
// upgrade happens first, so a device refusal arrives as a close reason (see
// readSessionID).
func (b *Bridge) openChannel(w http.ResponseWriter, r *http.Request) {
	socket, ok := socketTypeByName(r.URL.Query().Get("type"))
	if !ok {
		http.Error(w, "Bad Request", http.StatusBadRequest)
		return
	}

	// Keep the default same-origin check; it's what stops arbitrary web pages
	// reaching the device.
	conn, err := websocket.Accept(w, r, &websocket.AcceptOptions{
		// Already compressed or tiny.
		CompressionMode: websocket.CompressionDisabled,
	})
	if err != nil {
		b.Log.Debug("upgrade refused", "error", err)
		return
	}
	// Backstop for paths that don't close politely.
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

// readSessionID reads the session id from the first (text) message.
//
// Not in the URL because it's a credential (URLs get logged), it's base64
// with '+', '/' and '=', and browsers hide failed-handshake bodies, so only a
// close frame after upgrading can carry an error reason.
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

// relay copies bytes both ways until either side stops. Frames don't align
// with KVMMsg boundaries; the client reassembles the stream. Whichever
// direction ends first closes both.
func relay(ctx context.Context, conn *websocket.Conn, device net.Conn) error {
	browser := websocket.NetConn(ctx, conn, websocket.MessageBinary)

	done := make(chan error, 2)
	go func() { _, err := io.Copy(device, browser); done <- err }()
	go func() { _, err := io.Copy(browser, device); done <- err }()

	err := <-done
	// Unblock the other copy.
	_ = device.Close()
	_ = browser.Close()
	<-done

	if err == nil || errors.Is(err, io.EOF) || errors.Is(err, net.ErrClosed) {
		return nil
	}
	// A browser leaving is a normal end.
	if websocket.CloseStatus(err) != -1 || errors.Is(err, context.Canceled) {
		return nil
	}
	return err
}

// truncateReason fits a close reason into a control frame's 123 bytes.
func truncateReason(reason string) string {
	const max = 123
	if len(reason) <= max {
		return reason
	}
	// Must stay valid UTF-8.
	cut := max
	for cut > 0 && !utf8.RuneStart(reason[cut]) {
		cut--
	}
	return reason[:cut]
}
