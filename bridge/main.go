// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later

// Command netcmdr-bridge makes a Tripp Lite NetCommander / Minicom Smart IP KVM
// reachable from a browser.
//
// Browsers can't handshake with the device (no ECDHE, no TLS 1.3, 1024-bit
// DHE), so the bridge terminates its TLS and offers plain HTTP and WebSockets.
// The web client is embedded in the binary.
//
// # Security
//
// The bridge authenticates nobody, so anyone reaching it can reach the device.
// It listens on loopback by default; otherwise put it behind an authenticating
// TLS reverse proxy. The device address and ports are fixed at startup, so it
// is not an open relay.
package main

import (
	"context"
	"crypto/x509"
	"errors"
	"flag"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "netcmdr-bridge:", err)
		os.Exit(1)
	}
}

func run() error {
	var (
		host        = flag.String("host", os.Getenv("NETCMDR_HOST"), "Device address. [$NETCMDR_HOST]")
		httpsPort   = flag.Uint("https-port", 443, "Device management (HTTPS) port.")
		sessionPort = flag.Uint("session-port", 900, "Device session port. The login reply advertises this; override it if the device has been moved off the default.")
		listen      = flag.String("listen", "127.0.0.1:8080", "Address to listen on.")
		timeout     = flag.Uint("timeout", 20, "How long to wait on the device, in `SECONDS`.")
		insecure    = flag.Bool("insecure", envBool("NETCMDR_INSECURE"), "Skip checking the device's TLS certificate, e.g. for the factory self-signed one. [$NETCMDR_INSECURE]")
		caFile      = flag.String("ca-file", os.Getenv("NETCMDR_CA_FILE"), "Also trust the certificates in this PEM `FILE`: a private CA, the intermediate the device doesn't send, or the device's own certificate. [$NETCMDR_CA_FILE]")
	)
	flag.Usage = usage
	flag.Parse()

	if *host == "" {
		flag.Usage()
		return errors.New("--host is required")
	}
	if *httpsPort > 65535 || *sessionPort > 65535 {
		return errors.New("port numbers must be below 65536")
	}

	rootCAs, err := loadRootCAs(*caFile)
	if err != nil {
		return err
	}

	log := slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: logLevel()}))

	// Unpack the client before serving.
	assets, err := LoadAssets()
	if err != nil {
		return err
	}

	bridge := &Bridge{
		Device: &Device{
			Host:        *host,
			HTTPSPort:   uint16(*httpsPort),
			SessionPort: uint16(*sessionPort),
			Timeout:     time.Duration(*timeout) * time.Second,
			RootCAs:     rootCAs,
			Insecure:    *insecure,
		},
		Assets: assets,
		Log:    log,
	}

	listener, err := net.Listen("tcp", *listen)
	if err != nil {
		return fmt.Errorf("listening on %s: %w", *listen, err)
	}
	bound := listener.Addr().(*net.TCPAddr)

	if bound.IP.IsLoopback() {
		log.Info("bridge ready", "bound", bound, "device", bridge.Device.Host)
	} else {
		log.Warn("listening off-loopback: anything that can reach this can reach the device",
			"bound", bound, "device", bridge.Device.Host)
	}
	if assets.Empty() {
		log.Warn("this build has no web client in it: serving the protocol routes only. " +
			"Build the client with tools/build-web.sh, pack it with tools/pack-web.sh and build the bridge again.")
	}

	server := &http.Server{
		Handler: bridge.Handler(),
		// Only headers get a deadline; relays are long-lived.
		ReadHeaderTimeout: bridge.Device.Timeout,
		ErrorLog:          slog.NewLogLogger(log.Handler(), slog.LevelDebug),
	}

	// Exit promptly on SIGTERM (e.g. as PID 1 in a container). Connections
	// hold no state worth draining.
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGTERM, syscall.SIGINT)
	defer stop()

	go func() {
		<-ctx.Done()
		log.Info("stopping")
		if err := server.Close(); err != nil {
			log.Warn("closing the listener failed", "error", err)
		}
	}()

	if err := server.Serve(listener); err != nil && !errors.Is(err, http.ErrServerClosed) {
		return err
	}
	return nil
}

// loadRootCAs is the system's roots plus those in path, or nil (the system's)
// if path is empty.
func loadRootCAs(path string) (*x509.CertPool, error) {
	if path == "" {
		return nil, nil
	}
	pem, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	// Without system roots, the file alone is trusted.
	pool, err := x509.SystemCertPool()
	if err != nil {
		pool = x509.NewCertPool()
	}
	if !pool.AppendCertsFromPEM(pem) {
		return nil, fmt.Errorf("%s has no PEM certificates", path)
	}
	return pool, nil
}

// envBool reads a yes/no environment variable; unset or unparsable is false.
func envBool(name string) bool {
	switch strings.ToLower(os.Getenv(name)) {
	case "1", "true", "yes", "on":
		return true
	}
	return false
}

// logLevel reads NETCMDR_LOG.
func logLevel() slog.Level {
	level := slog.LevelInfo
	if name := os.Getenv("NETCMDR_LOG"); name != "" {
		if err := level.UnmarshalText([]byte(name)); err != nil {
			level = slog.LevelInfo
		}
	}
	return level
}

func usage() {
	out := flag.CommandLine.Output()
	// Nowhere to report a failed usage write.
	_, _ = fmt.Fprint(out, `netcmdr-bridge — serve the web client for a NetCommander IP KVM and relay its protocol

Terminates the device's legacy TLS — which no browser can negotiate — and offers
the same protocol over plain HTTP and WebSockets, so a browser can run the KVM
client, which is built into this binary.

The bridge authenticates nobody: reaching it means reaching the device. Keep the
default loopback listen address unless something in front of it is doing the
authenticating.

Usage:
  netcmdr-bridge --host <address> [options]

Options:
`)
	flag.PrintDefaults()
	_, _ = fmt.Fprint(out, "\nNETCMDR_LOG sets the log level (debug, info, warn, error).\n")
}
