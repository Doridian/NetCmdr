// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later

package main

import (
	"bufio"
	"crypto/rand"
	"crypto/rsa"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/binary"
	"io"
	"math/big"
	"net"
	"strings"
	"testing"
	"time"
)

func TestChannelNamesMapToTheProtocolsSocketTypes(t *testing.T) {
	for name, want := range map[string]SocketType{
		"cmd":   SocketCmd,
		"video": SocketVideo,
		"km":    SocketKeyboardMouse,
		"vm":    SocketVirtualMedia,
	} {
		got, ok := socketTypeByName(name)
		if !ok || got != want {
			t.Errorf("socketTypeByName(%q) = %v, %v; want %v, true", name, got, ok, want)
		}
	}
}

// The relay must not be steerable by name; an unknown one is a refusal, never a
// fallback.
func TestAnUnknownChannelNameIsRefused(t *testing.T) {
	for _, name := range []string{"general", "", "0", "CMD"} {
		if _, ok := socketTypeByName(name); ok {
			t.Errorf("socketTypeByName(%q) was accepted", name)
		}
	}
}

func TestOnlyTheCommandAndKeyboardChannelsUseTLS(t *testing.T) {
	if !SocketCmd.usesTLS() || !SocketKeyboardMouse.usesTLS() {
		t.Error("the command and keyboard/mouse channels must start TLS")
	}
	if SocketVideo.usesTLS() || SocketVirtualMedia.usesTLS() {
		t.Error("video and virtual media are in the clear by the device's design")
	}
}

func TestParsesBodyFromResponse(t *testing.T) {
	raw := []byte("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-length: 5\r\n\r\nhello")
	body, err := parseHTTPResponse(raw)
	if err != nil || body != "hello" {
		t.Fatalf("got %q, %v; want \"hello\", nil", body, err)
	}
}

func TestSurfacesNon200(t *testing.T) {
	_, err := parseHTTPResponse([]byte("HTTP/1.1 400 Bad Request\r\nContent-length: 0\r\n\r\n"))
	if err == nil || !strings.Contains(err.Error(), "400") {
		t.Fatalf("got %v; want an error naming 400", err)
	}
}

func TestRejectsResponseWithoutSeparator(t *testing.T) {
	if _, err := parseHTTPResponse([]byte("HTTP/1.1 200 OK")); err == nil {
		t.Fatal("a response with no header/body separator must be an error")
	}
}

// selfSigned is a certificate like the device's: self-signed, SHA-256, RSA-2048.
func selfSigned(t *testing.T) tls.Certificate {
	t.Helper()
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatal(err)
	}
	template := x509.Certificate{
		SerialNumber: big.NewInt(1),
		Subject:      pkix.Name{CommonName: "kvm-rack"},
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().Add(time.Hour),
		IPAddresses:  []net.IP{net.ParseIP("127.0.0.1")},
	}
	der, err := x509.CreateCertificate(rand.Reader, &template, &template, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	return tls.Certificate{Certificate: [][]byte{der}, PrivateKey: key}
}

// deviceLikeTLS offers what the device offers Go: TLS 1.2, static RSA.
func deviceLikeTLS(t *testing.T) *tls.Config {
	return &tls.Config{
		Certificates: []tls.Certificate{selfSigned(t)},
		MinVersion:   tls.VersionTLS12,
		MaxVersion:   tls.VersionTLS12,
		CipherSuites: []uint16{tls.TLS_RSA_WITH_AES_256_GCM_SHA384},
	}
}

// Would fail first if Go dropped RSA key exchange.
func TestPostMessagePVAgainstADeviceLikeServer(t *testing.T) {
	ln, err := tls.Listen("tcp", "127.0.0.1:0", deviceLikeTLS(t))
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = ln.Close() }()

	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		defer func() { _ = conn.Close() }()
		head, _ := bufio.NewReader(conn).ReadString('\n')
		if !strings.HasPrefix(head, "POST /message.pv ") {
			return
		}
		_, _ = io.WriteString(conn, "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n<PV_AUTH/>")
	}()

	addr := ln.Addr().(*net.TCPAddr)
	device := &Device{Host: "127.0.0.1", HTTPSPort: uint16(addr.Port), Timeout: 5 * time.Second}
	body, err := device.PostMessagePV("<CMA_Message/>")
	if err != nil {
		t.Fatalf("the device's TLS profile must be reachable: %v", err)
	}
	if body != "<PV_AUTH/>" {
		t.Fatalf("got %q; want the device's answer", body)
	}
}

// fakeDevice does the session handshake like the device, then runs script.
func fakeDevice(t *testing.T, want SocketType, serverTLS *tls.Config, script func(net.Conn, *bufio.Reader)) uint16 {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = ln.Close() })

	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		defer func() { _ = conn.Close() }()

		br := bufio.NewReader(conn)
		id, err := br.ReadString('#')
		if err != nil {
			return
		}
		if id != "testsession#" {
			t.Errorf("preamble was %q; want \"testsession#\"", id)
			return
		}
		var code [4]byte
		if _, err := io.ReadFull(br, code[:]); err != nil {
			return
		}
		if got := SocketType(binary.LittleEndian.Uint32(code[:])); got != want {
			t.Errorf("socket type was %d; want %d", got, want)
			return
		}
		if _, err := conn.Write(code[:]); err != nil {
			return
		}

		if serverTLS != nil {
			// Pipelined bytes belong to the TLS stream.
			server := tls.Server(bufferedConn{Conn: conn, Reader: br}, serverTLS)
			if err := server.Handshake(); err != nil {
				return
			}
			defer func() { _ = server.Close() }()
			script(server, bufio.NewReader(server))
			return
		}
		script(conn, br)
	}()

	return uint16(ln.Addr().(*net.TCPAddr).Port)
}

// bufferedConn lets a TLS server read bytes a bufio.Reader already swallowed.
type bufferedConn struct {
	net.Conn
	*bufio.Reader
}

func (c bufferedConn) Read(p []byte) (int, error) { return c.Reader.Read(p) }

func TestDialSessionCompletesThePlaintextHandshake(t *testing.T) {
	port := fakeDevice(t, SocketVideo, nil, func(conn net.Conn, _ *bufio.Reader) {
		_, _ = conn.Write([]byte("frame"))
	})

	device := &Device{Host: "127.0.0.1", SessionPort: port, Timeout: 5 * time.Second}
	conn, err := device.DialSession("testsession", SocketVideo)
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = conn.Close() }()

	got := make([]byte, 5)
	if _, err := io.ReadFull(conn, got); err != nil {
		t.Fatal(err)
	}
	if string(got) != "frame" {
		t.Fatalf("got %q; want \"frame\"", got)
	}
}

// The command channel upgrades to TLS after the same plaintext handshake.
func TestDialSessionUpgradesTheCommandChannelToTLS(t *testing.T) {
	port := fakeDevice(t, SocketCmd, deviceLikeTLS(t), func(conn net.Conn, _ *bufio.Reader) {
		_, _ = conn.Write([]byte("secret"))
	})

	device := &Device{Host: "127.0.0.1", SessionPort: port, Timeout: 5 * time.Second}
	conn, err := device.DialSession("testsession", SocketCmd)
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = conn.Close() }()

	if _, ok := conn.(*tls.Conn); !ok {
		t.Fatalf("the command channel must be TLS, got %T", conn)
	}
	got := make([]byte, 6)
	if _, err := io.ReadFull(conn, got); err != nil {
		t.Fatal(err)
	}
	if string(got) != "secret" {
		t.Fatalf("got %q; want \"secret\"", got)
	}
}

// A device that echoes the wrong code is a failed handshake, not a channel.
func TestDialSessionRefusesAMismatchedEcho(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = ln.Close() }()
	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		defer func() { _ = conn.Close() }()
		br := bufio.NewReader(conn)
		_, _ = br.ReadString('#')
		var code [4]byte
		_, _ = io.ReadFull(br, code[:])
		_, _ = conn.Write([]byte{99, 0, 0, 0})
	}()

	port := uint16(ln.Addr().(*net.TCPAddr).Port)
	device := &Device{Host: "127.0.0.1", SessionPort: port, Timeout: 5 * time.Second}
	if _, err := device.DialSession("testsession", SocketVideo); err == nil {
		t.Fatal("a mismatched echo must fail the handshake")
	}
}
