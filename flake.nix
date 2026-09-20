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

        # Only what a Rust build reads, so that editing the README, the bridge
        # or this file does not rebuild every crate.
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

        # The Go bridge, and the script that packs the web client into the
        # directory it embeds. Kept apart from `src` so the two halves of the
        # tree do not rebuild each other.
        bridgeSrc = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./bridge
            ./tools/pack-web.sh
          ];
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
          # netcmdr-web compiles to nothing off wasm32, so it is not listed.
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

            # Nothing is compressed here: tools/pack-web.sh is what gzips the
            # client for the bridge, so a rebuild cannot leave a stale copy
            # next to a file.

            runHook postInstall
          '';

          meta = {
            description = "Browser client for Tripp Lite NetCommander / Minicom Smart IP KVM switches";
            homepage = "https://git.foxden.network/FoxDen/NetCmdr";
            license = lib.licenses.agpl3Plus;
          };
        };

        # What OCI calls each architecture the image is published for. It is
        # written into the image's config and it is what a manifest list is
        # assembled out of. It is also Go's name for the architecture, so it is
        # GOARCH as well — which is the whole of what cross-compiling the bridge
        # takes.
        ociArch = {
          x86_64-linux = "amd64";
          aarch64-linux = "arm64";
          x86_64-darwin = "amd64";
          aarch64-darwin = "arm64";
        };

        # `e_machine`, at offset 18 of an ELF header. The build below reads it
        # back off the binary it just produced, because a GOARCH that failed to
        # take is otherwise silent: the result is this machine's architecture
        # under another one's name, and the image then advertises an
        # architecture it cannot run.
        elfMachine = {
          amd64 = 62;
          arm64 = 183;
        };

        # Go's own name for this machine, which is what a build that names no
        # target is for.
        hostGoos = if pkgs.stdenv.hostPlatform.isDarwin then "darwin" else "linux";

        # The bridge, with the web client baked into it. The binary is the whole
        # service: there is nothing to deploy, mount or point it at beside the
        # executable.
        #
        # This is Go rather than Rust so that the images below cost nothing to
        # cross-compile. CGO is off and crypto/tls stands in for OpenSSL, so the
        # result is static with no libc to match, no second toolchain to
        # bootstrap and no store path to scrub out — GOARCH is the entire cross
        # story. What that costs is forward secrecy on the management channel;
        # see README.md#tls and bridge/device.go.
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

            # The dependencies are fetched rather than committed here, so there
            # is a hash to keep in step with go.mod. It is a file of its own
            # because that is where Renovate's post-upgrade task writes it when
            # it bumps one; see renovate-local.json.
            vendorHash = builtins.readFile ./bridge/vendor-hash.txt;

            env.CGO_ENABLED = "0";
            ldflags = [
              "-s"
              "-w"
            ];
            # The linker has already dropped the symbol table and the debug
            # information, and this machine's `strip` has no business on a binary
            # built for another architecture.
            dontStrip = true;

            # The client is packed in before the compiler runs. Not in postPatch,
            # which the derivation behind vendorHash inherits: a set of Go
            # modules has no business depending on the wasm client, and fetching
            # them would otherwise have to build it first.
            preConfigure = ''
              sh tools/pack-web.sh ${web} bridge/web/dist
            '';

            # Exported here rather than passed in `env`, where buildGoModule sets
            # both from the Go it builds with. This way the target leaks into one
            # place and the module fetch stays the same derivation for every
            # architecture, rather than one apiece for a module set that does not
            # differ between them.
            postConfigure = ''
              export GOOS=${goos} GOARCH=${goarch}
            '';

            # Go puts a build for another architecture in a subdirectory of its
            # own, and the normalisation nixpkgs does for that only fires when
            # the Nix platforms differ — which here they do not, GOARCH being the
            # whole of the cross build.
            postBuild = lib.optionalString (!native) ''
              mv "$GOPATH/bin/${goos}_${goarch}"/* "$GOPATH/bin"
              rmdir "$GOPATH/bin/${goos}_${goarch}"
            '';

            # `go install` names a binary after the import path of its package,
            # and this one's is the module root.
            postInstall = ''
              mv "$out/bin/bridge" "$out/bin/netcmdr-bridge"
            '';

            # Only where the result can actually run: nothing here can execute a
            # binary for another architecture, so a cross build is not tested
            # and does not pretend to be.
            doCheck = native;

            # Trust the binary, not the variable. See elfMachine above.
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

        # That binary and nothing else, as an image: no base layer, no shell, no
        # store. `docker load < result` and run it.
        #
        # A Go binary with CGO off has no interpreter, no DT_NEEDED and no store
        # path inside it, and the web client is already part of the executable.
        # The device's certificate is never verified, so nothing outside the file
        # is read at runtime but /etc/resolv.conf, and only if --host is given a
        # name. That is a container built FROM scratch.
        #
        # The device goes in the environment rather than the command, so that
        # passing arguments — a different port, a longer timeout — does not
        # silently drop it along with the rest of the default command.
        mkBridgeImage =
          { goarch, tag }:
          pkgs.dockerTools.buildImage {
            name = "netcmdr-bridge";
            inherit tag;
            # Without this a cross-built image would still claim the
            # architecture of the machine that built it, and whatever pulled it
            # would refuse to run it or, worse, try to.
            architecture = goarch;

            # Copied rather than linked in: a symlink into /nix/store would
            # dangle in an image that has no store to point at.
            #
            # The copy is then scrubbed of store paths. nixpkgs patches Go's
            # standard library to find its data files in the store rather than
            # under /etc — iana-etc for `net`, mailcap for `mime`, tzdata for
            # `time` — and following those references would otherwise drag all
            # three into the image, tzdata's several megabytes of zoneinfo
            # included. None of them is read: the ports this dials are numeric
            # so no service name is ever looked up, the content types it serves
            # come from its own table, and a container keeps UTC.
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
                '';

            config = {
              Entrypoint = [ "/bin/netcmdr-bridge" ];
              # Loopback is the binary's default and the right one on a host; in
              # a namespace of its own it would mean nothing outside could reach
              # the bridge, so the image listens on all of the container's
              # addresses. What the warning it logs says still holds: whatever
              # can reach the published port can reach the device.
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

        # This machine's architecture, under the bare version tag: the one to
        # build and run by hand.
        bridgeImage = mkBridgeImage {
          goarch = ociArch.${system};
          tag = version;
        };
      in
      {
        # One shell for both halves of the tree, so that CI and a checkout lint
        # and test with the same toolchain the builds above use: the cargo
        # workspace needs pkg-config and OpenSSL to compile openssl-sys, and the
        # bridge needs Go. golangci-lint is here rather than fetched by CI so
        # that a run here and a run there are the same linter.
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

          # The viewer dlopens these, so the linker never records them and a
          # test that starts a window would not find them otherwise.
          LD_LIBRARY_PATH = lib.makeLibraryPath viewerLibraries;
        };

        packages = {
          default = native;
          netcmdr = native;
          netcmdr-bridge = bridge;
        }
        # dockerTools is Linux's; on macOS these do not even evaluate.
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
          {
            netcmdr-bridge-image = bridgeImage;
          }
          # The same image for each architecture it is published under, built
          # here whichever one this machine is — the bridge cross-compiles for
          # free, and the wasm client it embeds is the same derivation either
          # way, so it is compiled once rather than once per architecture. The
          # tag carries the architecture because CI loads both into one daemon,
          # where a shared name:tag would leave the first of them nameless.
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
