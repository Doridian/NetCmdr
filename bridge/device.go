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

// SocketType is one of the four session connections. Cmd and KeyboardMouse
// use TLS; Video and VirtualMedia don't. Mirrors
// crates/netcmdr-proto/src/kvmmsg.rs.
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

// socketTypeByName maps the "type" query parameter to a socket. Unknown names
// are refused.
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

// deviceTLS is the TLS config for this device.
//
// The device offers only DHE and static RSA, and Go has no DHE, so static RSA
// suites must be listed explicitly (Go 1.22 removed them from the defaults).
// This loses forward secrecy on /message.pv, matching the session sockets.
// See README.md#tls.
//
// The certificate is self-signed and not verified, as in the vendor client.
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

// Device is the bridge's fixed target.
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
		// Best effort.
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

// PostMessagePV posts body to /message.pv over a fresh connection and returns
// the response body. HTTP is hand-rolled because the device's unclean TLS
// shutdown would make net/http report an error.
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

	// An error after data is the unclean shutdown; the response is complete.
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
// Transcribed from com.minicom.plugin.socket.{KVMSocket,SocketUtils}.
// sessionID is the SESSIONID from the HTTPS login.
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
