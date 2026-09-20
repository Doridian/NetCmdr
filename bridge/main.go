// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later

// Command netcmdr-bridge puts a Tripp Lite NetCommander / Minicom Smart IP KVM
// behind something a browser can actually talk to.
//
// # Why this exists
//
// The device's TLS is the problem, and it is not a fixable one. It offers no
// ECDHE suite at all — only DHE and static RSA — no TLS 1.3, a 1024-bit DHE
// group and a self-signed certificate. A browser has no switch for any of that
// and never will. No amount of clicking through a certificate warning helps,
// because the handshake fails before a certificate is ever considered.
//
// So the bridge terminates the device's TLS itself and offers the browser plain
// HTTP and WebSockets. The web client then needs no TLS stack of its own, which
// is most of what makes a WebAssembly build of it possible.
//
// There is nothing to deploy beside the executable: the client is built into
// it, gzipped, and unpacked when the bridge starts.
//
// # Security
//
// The bridge does not authenticate anyone. It cannot: the only credential in
// this protocol is the device's own SESSIONID, and minting one is exactly what
// /message.pv is for. Anything that can reach the listening port can therefore
// reach the device's management API and session ports, including to attempt
// logins. That is why the default listen address is loopback, and why anything
// else should be put behind a reverse proxy that does authenticate, and that
// terminates a TLS the browser will accept.
//
// What it does *not* do is act as an open relay: the device address and both
// ports are fixed at startup and cannot be steered by a request.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
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

	log := slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: logLevel()}))

	// Unpack the built-in client before anything can ask for it, so that the
	// work is done while nothing is waiting on it.
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
		// A read or write deadline would cut a KVM session off mid-stream, so
		// only the head is on a clock. A relay lives as long as its channel.
		ReadHeaderTimeout: bridge.Device.Timeout,
		ErrorLog:          slog.NewLogLogger(log.Handler(), slog.LevelDebug),
	}

	// Stop when told to, rather than when killed. This matters most as PID 1 in
	// a container: without a handler, `docker stop` would wait out its whole
	// grace period and then SIGKILL the bridge.
	//
	// Nothing is lost by going out this way. A connection holds no state worth
	// finishing — the device's own session outlives any one of them — so the
	// listener is closed and the process leaves rather than draining.
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

// logLevel reads NETCMDR_LOG, so that the bridge can be turned up without being
// rebuilt.
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
	// Usage goes wherever flag was told to write; a failure there is the
	// caller's broken pipe, and there is nowhere left to report it to.
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
