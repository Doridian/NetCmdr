{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        inherit (pkgs) lib;

        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

        # Only what a build reads, so that editing the README or this file does
        # not rebuild every crate.
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./crates
            # The live captures netcmdr-mgmt's tests parse.
            ./research/notes/analysis/live-fixtures
          ];
        };
        cargoLock = {
          lockFile = ./Cargo.lock;
        };

        # What a crate is pinned to in Cargo.lock. wasm-bindgen's CLI has to be
        # the version the module was built against, so that figure comes from
        # the lock file rather than from a second place that could disagree.
        lockedVersion =
          name:
          (lib.findFirst (p: p.name == name) (throw "${name} is not in Cargo.lock")
            (builtins.fromTOML (builtins.readFile ./Cargo.lock)).package
          ).version;

        # The viewer reaches OpenGL, xkbcommon, Wayland and X11 through dlopen,
        # so the linker never sees them and the binary carries no DT_NEEDED for
        # any of them. They go on its RPATH instead. macOS draws through its own
        # frameworks and needs none of this.
        viewerLibraries = lib.optionals pkgs.stdenv.hostPlatform.isLinux (
          with pkgs;
          [
            libGL
            libxkbcommon
            wayland
            libx11
            libxcursor
            libxi
            libxrandr
          ]
        );

        # The management client and the desktop viewer: everything that runs on
        # this machine and talks to the device directly.
        native = pkgs.rustPlatform.buildRustPackage {
          pname = "netcmdr";
          inherit version src cargoLock;

          cargoBuildFlags = [
            "-p"
            "netcmdr-cli"
            "-p"
            "netcmdr-view"
          ];
          # Everything except the bridge, which is its own package below, and
          # netcmdr-web, which compiles to nothing off wasm32.
          cargoTestFlags = [
            "-p"
            "netcmdr-cli"
            "-p"
            "netcmdr-view"
            "-p"
            "netcmdr-ui"
            "-p"
            "netcmdr-mgmt"
            "-p"
            "netcmdr-proto"
            "-p"
            "netcmdr-session"
          ];

          nativeBuildInputs = [ pkgs.pkg-config ];
          # OpenSSL rather than rustls: the device offers no ECDHE suite and a
          # 1024-bit DHE group, which needs SECLEVEL=1. See README.md#tls.
          buildInputs = [ pkgs.openssl ] ++ viewerLibraries;

          postFixup = lib.optionalString (viewerLibraries != [ ]) ''
            patchelf --add-rpath ${lib.makeLibraryPath viewerLibraries} $out/bin/netcmdr-view
          '';

          meta = {
            description = "Management CLI and KVM viewer for Tripp Lite NetCommander / Minicom Smart IP KVM switches";
            homepage = "https://git.foxden.network/FoxDen/NetCmdr";
            license = lib.licenses.agpl3Plus;
            mainProgram = "netcmdr";
            platforms = lib.platforms.linux ++ lib.platforms.darwin;
          };
        };

        # wasm-bindgen's glue and the module agree on an ABI that is not stable
        # between releases, so the CLI must be the version of the crate the
        # module was compiled against. nixpkgs pins a handful of versions and
        # none of them need be the one in Cargo.lock, so build that one.
        wasmBindgenVersion = lockedVersion "wasm-bindgen";
        wasm-bindgen-cli = pkgs.buildWasmBindgenCli rec {
          src = pkgs.fetchCrate {
            pname = "wasm-bindgen-cli";
            version = wasmBindgenVersion;
            hash = "sha256-a7lcXJnnZkYReja+iUO7NqqrWyv3toxnUgQb8s4IS5s=";
          };
          cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
            inherit src;
            inherit (src) pname version;
            hash = "sha256-R1Tas33Ursy8kqsxguAkG0ZhNed2n5uFTAhw1l2qlLY=";
          };
        };

        # The browser client: the same viewer as netcmdr-view, compiled to
        # WebAssembly. This is what tools/build-web.sh produces by hand, and it
        # is what the bridge embeds.
        web = pkgs.rustPlatform.buildRustPackage {
          pname = "netcmdr-web";
          inherit version src cargoLock;

          nativeBuildInputs = [
            wasm-bindgen-cli
            pkgs.binaryen
            # rustc links wasm32-unknown-unknown with lld, which nixpkgs does
            # not ship inside its rustc.
            pkgs.lld
          ];

          # Nothing here runs on this machine, and stripping a .wasm with the
          # host's binutils is not something to hand a browser.
          doCheck = false;
          dontStrip = true;
          # cargo-auditable would write a dependency list into the module the
          # browser downloads, which is the one place that weight is not free.
          auditable = false;

          buildPhase = ''
            runHook preBuild

            locked=${lib.escapeShellArg wasmBindgenVersion}
            have=$(wasm-bindgen --version | cut -d' ' -f2)
            if [ "$locked" != "$have" ]; then
              echo "wasm-bindgen CLI is $have but Cargo.lock pins $locked;" >&2
              echo "update wasmBindgenVersion's hashes in flake.nix." >&2
              exit 1
            fi

            cargo build -j $NIX_BUILD_CORES --offline \
              -p netcmdr-web --target wasm32-unknown-unknown --profile web

            runHook postBuild
          '';

          installPhase = ''
            runHook preInstall

            mkdir -p $out
            wasm-bindgen \
              --target web \
              --no-typescript \
              --out-dir $out \
              target/wasm32-unknown-unknown/web/netcmdr_web.wasm

            # -Oz is size over speed, which is the right trade for a module the
            # browser downloads once. The feature flags are the ones rustc emits
            # by default; older binaryen refuses the module without them.
            wasm-opt -Oz \
              --enable-bulk-memory \
              --enable-nontrapping-float-to-int \
              --enable-sign-ext \
              --enable-mutable-globals \
              --enable-reference-types \
              --enable-multivalue \
              -o $out/netcmdr_web_bg.wasm.opt $out/netcmdr_web_bg.wasm
            mv $out/netcmdr_web_bg.wasm.opt $out/netcmdr_web_bg.wasm

            cp crates/netcmdr-web/web/index.html $out/

            # Nothing is compressed here: brotli is netcmdr-bridge's build script's
            # job, so a rebuild cannot leave a stale .br next to a file.

            runHook postInstall
          '';

          meta = {
            description = "Browser client for Tripp Lite NetCommander / Minicom Smart IP KVM switches";
            homepage = "https://git.foxden.network/FoxDen/NetCmdr";
            license = lib.licenses.agpl3Plus;
          };
        };

        # The bridge, with the client above baked into it. The build script
        # reads $NETCMDR_WEB_DIST, so the binary is the whole service: there is
        # nothing to deploy, mount or point it at beside the executable.
        #
        # Taken over a package set rather than written once, so that the static
        # build below is the same derivation against a different stdenv.
        mkBridge =
          p:
          let
            static = p.stdenv.hostPlatform.isStatic;
          in
          p.rustPlatform.buildRustPackage {
            # No suffix for the static one: a static stdenv already puts its own
            # on the derivation name.
            pname = "netcmdr-bridge";
            inherit version src cargoLock;

            cargoBuildFlags = [
              "-p"
              "netcmdr-bridge"
            ];
            cargoTestFlags = [
              "-p"
              "netcmdr-bridge"
            ];

            env = {
              NETCMDR_WEB_DIST = "${web}";
            }
            # openssl-sys links whatever pkg-config hands it, and for a static
            # stdenv that has to be the archives. It does not infer this from
            # the target, so say so.
            // lib.optionalAttrs static {
              OPENSSL_STATIC = "1";
            };

            nativeBuildInputs = [ p.pkg-config ];
            buildInputs = [ p.openssl ];

            meta = {
              description =
                "WebSocket and HTTPS bridge serving the browser client for Tripp Lite NetCommander / Minicom Smart IP KVM switches"
                + lib.optionalString static " (static build)";
              homepage = "https://git.foxden.network/FoxDen/NetCmdr";
              license = lib.licenses.agpl3Plus;
              mainProgram = "netcmdr-bridge";
              platforms = if static then lib.platforms.linux else lib.platforms.linux ++ lib.platforms.darwin;
            };
          };

        bridge = mkBridge pkgs;

        # The same bridge against musl with everything, OpenSSL included, linked
        # in: no interpreter, no DT_NEEDED, no libc to match. The web client is
        # already inside the executable, and the device's certificate is never
        # verified (see netcmdr-mgmt's transport), so nothing outside the file
        # is read at runtime but /etc/resolv.conf, and only if --host is given a
        # name. That is a container built FROM scratch.
        bridgeStatic = mkBridge pkgs.pkgsStatic;

        # That binary and nothing else, as an image: no base layer, no shell,
        # no store. `docker load < result` and run it.
        #
        # The device goes in the environment rather than the command, so that
        # passing arguments — a different port, a longer timeout — does not
        # silently drop it along with the rest of the default command.
        bridgeImage = pkgs.dockerTools.buildImage {
          name = "netcmdr-bridge";
          tag = version;

          # Copied rather than linked in: a symlink into /nix/store would dangle
          # in an image that has no store to point at.
          #
          # The copy is then scrubbed of store paths, because the only ones a
          # static OpenSSL leaves behind are the OPENSSLDIR and MODULESDIR it
          # was configured with, and following them would otherwise drag
          # openssl's etc into the image. Neither is read: no certificate is
          # verified, no provider is loaded from disk, and a handshake with the
          # device's profile — TLS 1.2, 1024-bit DHE at security level 1 —
          # completes with no openssl.cnf in reach.
          copyToRoot =
            pkgs.runCommand "netcmdr-bridge-root" { nativeBuildInputs = [ pkgs.nukeReferences ]; }
              ''
                mkdir -p $out/bin
                cp ${bridgeStatic}/bin/netcmdr-bridge $out/bin/netcmdr-bridge
                chmod +w $out/bin/netcmdr-bridge
                nuke-refs $out/bin/netcmdr-bridge
              '';

          config = {
            Entrypoint = [ "/bin/netcmdr-bridge" ];
            # Loopback is the binary's default and the right one on a host; in a
            # namespace of its own it would mean nothing outside could reach the
            # bridge, so the image listens on all of the container's addresses.
            # What the warning it logs says still holds: whatever can reach the
            # published port can reach the device.
            Cmd = [
              "--listen"
              "0.0.0.0:8080"
            ];
            ExposedPorts = {
              "8080/tcp" = { };
            };
            # Nothing here is read or written at runtime and the port is above
            # 1024, so there is nothing for root to do.
            User = "65534:65534";
          };
        };
      in
      {
        packages = {
          default = native;
          netcmdr = native;
          netcmdr-bridge = bridge;
        }
        # musl and dockerTools are Linux's; on macOS these do not even evaluate.
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          netcmdr-bridge-static = bridgeStatic;
          netcmdr-bridge-image = bridgeImage;
        };
      }
    );
}
