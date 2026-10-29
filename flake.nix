{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
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

        # Only Rust build inputs, to avoid needless rebuilds.
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./crates
            ./tools/wasm-bindgen-cli
            # The live captures netcmdr-mgmt's tests parse.
            ./research/notes/analysis/live-fixtures
          ];
        };
        cargoLock = {
          lockFile = ./Cargo.lock;
        };

        # The Go bridge and the web packing script, separate from `src`.
        bridgeSrc = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./bridge
            ./tools/pack-web.sh
          ];
        };

        # A crate's Cargo.lock entry (wasm-bindgen's CLI must match it).
        lockedPackage =
          name:
          lib.findFirst (p: p.name == name) (throw "${name} is not in Cargo.lock")
            (builtins.fromTOML (builtins.readFile ./Cargo.lock)).package;

        # The viewer dlopens GL, xkbcommon, Wayland and X11, so they go on its
        # RPATH. Not needed on macOS.
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

        # The management CLI and desktop viewer.
        native = pkgs.rustPlatform.buildRustPackage {
          pname = "netcmdr";
          inherit version src cargoLock;

          cargoBuildFlags = [
            "-p"
            "netcmdr-cli"
            "-p"
            "netcmdr-view"
          ];
          # netcmdr-web is empty off wasm32.
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
          # OpenSSL, not rustls; see README.md#tls.
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

        # wasm-bindgen CLI at the exact Cargo.lock version (unstable ABI).
        # tools/wasm-bindgen-cli pins it into Cargo.lock, which also holds
        # the library to the same version, so its checksum is the hash.
        wasmBindgenVersion = (lockedPackage "wasm-bindgen").version;
        wasmBindgenCliCrate = lockedPackage "wasm-bindgen-cli";
        wasm-bindgen-cli = pkgs.buildWasmBindgenCli rec {
          inherit (wasmBindgenCliCrate) version;
          src =
            pkgs.runCommand "wasm-bindgen-cli-${version}-source"
              {
                crate = pkgs.fetchurl {
                  name = "wasm-bindgen-cli-${version}.tar.gz";
                  url = "https://static.crates.io/crates/wasm-bindgen-cli/wasm-bindgen-cli-${version}.crate";
                  sha256 = wasmBindgenCliCrate.checksum;
                };
              }
              ''
                mkdir $out
                tar -xzf $crate -C $out --strip-components=1
              '';
          cargoDeps = pkgs.rustPlatform.importCargoLock {
            lockFile = "${src}/Cargo.lock";
          };
        };

        # The browser client (as tools/build-web.sh), embedded by the bridge.
        web = pkgs.rustPlatform.buildRustPackage {
          pname = "netcmdr-web";
          inherit version src cargoLock;

          nativeBuildInputs = [
            wasm-bindgen-cli
            pkgs.binaryen
            # nixpkgs' rustc lacks lld for wasm32.
            pkgs.lld
          ];

          # Don't strip .wasm with host binutils.
          doCheck = false;
          dontStrip = true;
          # Keep the dependency list out of the download.
          auditable = false;

          buildPhase = ''
            runHook preBuild

            locked=${lib.escapeShellArg wasmBindgenVersion}
            have=$(wasm-bindgen --version | cut -d' ' -f2)
            if [ "$locked" != "$have" ]; then
              echo "wasm-bindgen CLI is $have but Cargo.lock pins $locked;" >&2
              echo "bump tools/wasm-bindgen-cli's pin to match." >&2
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

            # Optimise for size. Older binaryen needs rustc's default features
            # spelled out.
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

            # tools/pack-web.sh does the gzipping.

            runHook postInstall
          '';

          meta = {
            description = "Browser client for Tripp Lite NetCommander / Minicom Smart IP KVM switches";
            homepage = "https://git.foxden.network/FoxDen/NetCmdr";
            license = lib.licenses.agpl3Plus;
          };
        };

        # OCI architecture names, which are also GOARCH values.
        ociArch = {
          x86_64-linux = "amd64";
          aarch64-linux = "arm64";
          x86_64-darwin = "amd64";
          aarch64-darwin = "arm64";
        };

        # ELF `e_machine` (offset 18), checked after the build because a
        # GOARCH that didn't take fails silently.
        elfMachine = {
          amd64 = 62;
          arm64 = 183;
        };

        # This machine's GOARCH.
        hostGoos = if pkgs.stdenv.hostPlatform.isDarwin then "darwin" else "linux";

        # The bridge with the web client embedded.
        #
        # Go (CGO off) so cross-compiling is just GOARCH and the binary is
        # static. The cost is forward secrecy on the management channel; see
        # README.md#tls.
        mkBridge =
          {
            goos ? hostGoos,
            goarch ? ociArch.${system},
          }:
          let
            native = goarch == ociArch.${system} && goos == hostGoos;
          in
          pkgs.buildGoModule {
            pname = "netcmdr-bridge";
            inherit version;
            src = bridgeSrc;
            modRoot = "bridge";

            # Separate file so Renovate can update it; see renovate-local.json.
            vendorHash = builtins.readFile ./bridge/vendor-hash.txt;

            env.CGO_ENABLED = "0";
            ldflags = [
              "-s"
              "-w"
            ];
            # Already stripped by the linker; host strip can't handle cross builds.
            dontStrip = true;

            # Not in postPatch, which the vendor derivation inherits.
            preConfigure = ''
              sh tools/pack-web.sh ${web} bridge/web/dist
            '';

            # Exported here, not via `env`, so the module fetch is shared
            # across architectures.
            postConfigure = ''
              export GOOS=${goos} GOARCH=${goarch}
            '';

            # Go puts cross builds in a subdirectory; nixpkgs doesn't handle
            # that when the Nix platforms match.
            postBuild = lib.optionalString (!native) ''
              mv "$GOPATH/bin/${goos}_${goarch}"/* "$GOPATH/bin"
              rmdir "$GOPATH/bin/${goos}_${goarch}"
            '';

            # Named after the module root.
            postInstall = ''
              mv "$out/bin/bridge" "$out/bin/netcmdr-bridge"
            '';

            # Cross builds can't run tests.
            doCheck = native;

            # See elfMachine.
            postFixup = lib.optionalString (goos == "linux") ''
              want=${toString elfMachine.${goarch}}
              got=$(od -An -tu2 -j18 -N2 "$out/bin/netcmdr-bridge" | tr -d ' ')
              if [ "$got" != "$want" ]; then
                echo "GOARCH=${goarch} wanted ELF machine $want but the binary is $got;" >&2
                echo "the cross build did not take." >&2
                exit 1
              fi
            '';

            meta = {
              description = "WebSocket and HTTPS bridge serving the browser client for Tripp Lite NetCommander / Minicom Smart IP KVM switches";
              homepage = "https://git.foxden.network/FoxDen/NetCmdr";
              license = lib.licenses.agpl3Plus;
              mainProgram = "netcmdr-bridge";
              platforms = lib.platforms.linux ++ lib.platforms.darwin;
            };
          };

        bridge = mkBridge { };

        # A FROM-scratch image of just the binary. `docker load < result`.
        #
        # The device is set via the environment so extra arguments don't
        # replace it.
        mkBridgeImage =
          { goarch, tag }:
          pkgs.dockerTools.buildImage {
            name = "netcmdr-bridge";
            inherit tag;
            # Otherwise cross-built images claim the host's architecture.
            architecture = goarch;

            # Copied (a store symlink would dangle), then scrubbed of the store
            # paths nixpkgs patches into Go's stdlib (iana-etc, mailcap,
            # tzdata), none of which are used. The CA bundle is where Go looks
            # first, for verifying the device's certificate.
            copyToRoot =
              pkgs.runCommand "netcmdr-bridge-root-${goarch}" { nativeBuildInputs = [ pkgs.nukeReferences ]; }
                ''
                  install -Dm755 ${
                    mkBridge {
                      goos = "linux";
                      inherit goarch;
                    }
                  }/bin/netcmdr-bridge $out/bin/netcmdr-bridge
                  nuke-refs $out/bin/netcmdr-bridge
                  install -Dm644 ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt $out/etc/ssl/certs/ca-certificates.crt
                '';

            config = {
              Entrypoint = [ "/bin/netcmdr-bridge" ];
              # Loopback is useless in a container. Anything reaching the
              # published port can reach the device.
              Cmd = [
                "--listen"
                "0.0.0.0:8080"
              ];
              ExposedPorts = {
                "8080/tcp" = { };
              };
              # No root needed.
              User = "65534:65534";
            };
          };

        # This machine's architecture, under the bare version tag.
        bridgeImage = mkBridgeImage {
          goarch = ociArch.${system};
          tag = version;
        };
      in
      {
        # One dev shell for Rust and Go, shared with CI.
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.go
            pkgs.golangci-lint
          ];
          buildInputs = [ pkgs.openssl ] ++ viewerLibraries;

          # dlopened by the viewer.
          LD_LIBRARY_PATH = lib.makeLibraryPath viewerLibraries;
        };

        packages = {
          default = native;
          netcmdr = native;
          netcmdr-bridge = bridge;
        }
        # dockerTools is Linux-only.
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
          {
            netcmdr-bridge-image = bridgeImage;
          }
          # One image per architecture, cross-compiled from any host. Tags
          # include the architecture so both can be loaded at once.
          // lib.mapAttrs' (target: arch: {
            name = "netcmdr-bridge-image-${arch}";
            value = mkBridgeImage {
              goarch = arch;
              tag = "${version}-${arch}";
            };
          }) (lib.filterAttrs (target: _: lib.hasSuffix "-linux" target) ociArch)
        );
      }
    );
}
