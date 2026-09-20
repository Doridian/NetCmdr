// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later

package main

import (
	"crypto/tls"
	"encoding/binary"
	"fmt"
	"io"
	"net"
	"strconv"
	"strings"
	"time"
)

// SocketType is which of the four session connections a socket is.
//
// The client makes all four to the same port. Cmd and KeyboardMouse upgrade to
// TLS; Video and VirtualMedia run in the clear. That is the device's design,
// not a shortcut taken here. Transcribed from
// com.minicom.plugin.data.SocketType, and kept in step with
// crates/netcmdr-proto/src/kvmmsg.rs, which is authoritative.
type SocketType int32

const (
	SocketCmd           SocketType = 0
	SocketKeyboardMouse SocketType = 1
	SocketVideo         SocketType = 2
	SocketVirtualMedia  SocketType = 3
)

// usesTLS reports whether this socket starts TLS after the handshake.
func (s SocketType) usesTLS() bool {
	return s == SocketCmd || s == SocketKeyboardMouse
}

// socketTypeByName maps the "type" query parameter onto a socket, naming the
// channels as the protocol write-up names them rather than by their wire codes.
//
// The relay must not be steerable by name: an unknown one is a refusal, never a
// fallback.
func socketTypeByName(name string) (SocketType, bool) {
	switch name {
	case "cmd":
		return SocketCmd, true
	case "video":
		return SocketVideo, true
	case "km":
		return SocketKeyboardMouse, true
	case "vm":
		return SocketVirtualMedia, true
	}
	return 0, false
}

// deviceTLS is the only TLS configuration that reaches this device.
//
// The device (Embedthis Appweb 2.4.x on Windows CE) offers no ECDHE suite at
// all and no TLS 1.3 — only DHE and static RSA. Go's crypto/tls has never
// implemented finite-field DHE and offers no way to add it, so the static RSA
// suites are the whole of the overlap, and they have to be named explicitly:
// Go 1.22 dropped RSA key exchange from the default list, and a nil
// CipherSuites here fails the handshake outright.
//
// What this costs is forward secrecy on the management channel, where the Rust
// client keeps it by asking OpenSSL for security level 1 and negotiating DHE
// over the device's 1024-bit group. The session sockets on port 900 already
// negotiate AES256-GCM-SHA384 — static RSA — with either client, so this only
// moves /message.pv onto the footing the command and keyboard channels were
// always on. See README.md#tls.
//
// Verification is off: the certificate is self-signed with no trust anchor, and
// the vendor's own Java client verifies nothing either (SocketUtils installs a
// trust-all X509TrustManager). So this is no weaker than stock — but it does
// mean the session is only as private as the network it crosses.
func deviceTLS() *tls.Config {
	return &tls.Config{
		InsecureSkipVerify: true,
		MinVersion:         tls.VersionTLS12,
		CipherSuites: []uint16{
			tls.TLS_RSA_WITH_AES_256_GCM_SHA384,
			tls.TLS_RSA_WITH_AES_128_GCM_SHA256,
			tls.TLS_RSA_WITH_AES_256_CBC_SHA,
			tls.TLS_RSA_WITH_AES_128_CBC_SHA,
		},
	}
}

// Device is the one device this bridge is pointed at. The address and both
// ports are fixed at startup and cannot be steered by a request, which is what
// keeps the bridge from being an open relay.
type Device struct {
	Host        string
	HTTPSPort   uint16
	SessionPort uint16
	Timeout     time.Duration
}

// dialTLS opens a TCP connection to addr and completes the device's handshake.
func (d *Device) dialTLS(addr string) (*tls.Conn, error) {
	raw, err := net.DialTimeout("tcp", addr, d.Timeout)
	if err != nil {
		return nil, err
	}
	if tcp, ok := raw.(*net.TCPConn); ok {
		// Nagle off is an optimisation, not a requirement: a device that
		// refuses it still speaks the protocol correctly.
		_ = tcp.SetNoDelay(true)
	}
	if err := raw.SetDeadline(time.Now().Add(d.Timeout)); err != nil {
		_ = raw.Close()
		return nil, err
	}
	conn := tls.Client(raw, deviceTLS())
	if err := conn.Handshake(); err != nil {
		_ = raw.Close()
		return nil, fmt.Errorf("tls: %w", err)
	}
	return conn, nil
}

// PostMessagePV posts body to /message.pv and returns the response body.
//
// A fresh TLS connection is made per call. The device's session state lives in
// the SESSIONID carried inside the XML, not in the transport, so there is
// nothing to keep open between requests.
//
// The HTTP is hand-rolled rather than handed to net/http, for the same reason
// the Rust client hand-rolls it: the request is one POST with a small XML body,
// the server honours Connection: close, and a close_notify from a CE device can
// surface as an unclean shutdown that a stricter client would report as an
// error after it already had every byte of the answer.
func (d *Device) PostMessagePV(body string) (string, error) {
	addr := net.JoinHostPort(d.Host, strconv.Itoa(int(d.HTTPSPort)))
	conn, err := d.dialTLS(addr)
	if err != nil {
		return "", err
	}
	defer func() { _ = conn.Close() }()

	request := "POST /message.pv HTTP/1.1\r\n" +
		"Host: " + d.Host + "\r\n" +
		"Content-Type: application/x-www-form-urlencoded\r\n" +
		"Content-Length: " + strconv.Itoa(len(body)) + "\r\n" +
		"Cache-Control: no-cache,max-age=0\r\n" +
		"Connection: close\r\n" +
		"\r\n" + body
	if _, err := io.WriteString(conn, request); err != nil {
		return "", err
	}

	// Read to EOF: the server closes because we asked it to. An error with
	// bytes already in hand is the unclean shutdown described above, and the
	// response is complete by then.
	raw, err := io.ReadAll(conn)
	if err != nil && len(raw) == 0 {
		return "", err
	}
	return parseHTTPResponse(raw)
}

// parseHTTPResponse splits an HTTP/1.x response and returns its body.
func parseHTTPResponse(raw []byte) (string, error) {
	head, body, found := strings.Cut(string(raw), "\r\n\r\n")
	if !found {
		return "", fmt.Errorf("device: no header/body separator")
	}

	statusLine, _, _ := strings.Cut(head, "\n")
	fields := strings.Fields(strings.TrimSpace(statusLine))
	if len(fields) < 2 {
		return "", fmt.Errorf("device: no status code")
	}
	code, err := strconv.Atoi(fields[1])
	if err != nil {
		return "", fmt.Errorf("device: no status code")
	}
	if code != 200 {
		return "", fmt.Errorf("device: HTTP %d", code)
	}
	return body, nil
}

// DialSession opens one session socket and completes the device's handshake:
//
//  1. write the management session id followed by '#', as ASCII,
//  2. write the socket-type code as a 4-byte little-endian integer,
//  3. read 4 bytes back and check the device echoed the same code,
//  4. for the command and keyboard/mouse sockets only, start TLS.
//
// Transcribed from com.minicom.plugin.socket.{KVMSocket,SocketUtils}. This is
// the whole device-specific part of opening a channel, and the only place that
// decides whether TLS is started.
//
// sessionID is the SESSIONID from the HTTPS login — the same token the
// management API returns.
func (d *Device) DialSession(sessionID string, socket SocketType) (net.Conn, error) {
	addr := net.JoinHostPort(d.Host, strconv.Itoa(int(d.SessionPort)))
	raw, err := net.DialTimeout("tcp", addr, d.Timeout)
	if err != nil {
		return nil, err
	}
	if tcp, ok := raw.(*net.TCPConn); ok {
		_ = tcp.SetNoDelay(true)
	}
	if err := raw.SetDeadline(time.Now().Add(d.Timeout)); err != nil {
		_ = raw.Close()
		return nil, err
	}

	code := int32(socket)
	hello := make([]byte, 0, len(sessionID)+5)
	hello = append(hello, sessionID...)
	hello = append(hello, '#')
	hello = binary.LittleEndian.AppendUint32(hello, uint32(code))
	if _, err := raw.Write(hello); err != nil {
		_ = raw.Close()
		return nil, err
	}

	var echo [4]byte
	if _, err := io.ReadFull(raw, echo[:]); err != nil {
		_ = raw.Close()
		return nil, err
	}
	if echoed := int32(binary.LittleEndian.Uint32(echo[:])); echoed != code {
		_ = raw.Close()
		return nil, fmt.Errorf("handshake: expected socket type %d, device echoed %d", code, echoed)
	}

	if !socket.usesTLS() {
		// The relay sets its own deadlines from here on.
		if err := raw.SetDeadline(time.Time{}); err != nil {
			_ = raw.Close()
			return nil, err
		}
		return raw, nil
	}

	conn := tls.Client(raw, deviceTLS())
	if err := conn.Handshake(); err != nil {
		_ = raw.Close()
		return nil, fmt.Errorf("tls: %w", err)
	}
	if err := raw.SetDeadline(time.Time{}); err != nil {
		_ = conn.Close()
		return nil, err
	}
	return conn, nil
}
