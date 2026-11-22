{
  description = "esp-xip-modules host build dependencies";

  # The other flakes are tool dependencies: ssh-mesh for `mesh-api-gen` (scripts/gen.sh) and dmesh for `dmesh-cli` (talks
  # to a device over UART/USB with the PPP framing, the equivalent of the console over any transport a Linux host has). Nothing of this
  # project is built from, or linked with, ssh-mesh or dmesh.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    ssh-mesh.url = "github:costinm/ssh-mesh";
    dmesh = {
      url = "github:costinm/dmesh";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Rust toolchains with extra targets (wasm32): only for the `wasm-dev` package.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, nixpkgs, ssh-mesh, dmesh, rust-overlay }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          module-tools = pkgs.stdenvNoCC.mkDerivation {
            pname = "xip-module-tools";
            version = "0.1.0";
            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter = path: type: !(builtins.elem (baseNameOf path) [ "target" ".git" ]);
            };
            nativeBuildInputs = [ pkgs.makeWrapper ];
            installPhase = ''
              mkdir -p $out/share/esp-xip-modules/{modules,crates} $out/bin
              cp -r tools include $out/share/esp-xip-modules/
              cp -r modules/common $out/share/esp-xip-modules/modules/
              cp -r crates/xip-abi $out/share/esp-xip-modules/crates/
              makeWrapper ${pkgs.python3}/bin/python3 $out/bin/build-mod \
                --add-flags $out/share/esp-xip-modules/tools/build_mod.py \
                --set XIP_TOOL_ROOT $out/share/esp-xip-modules
            '';
          };
          # ESP-IDF v6.0.2, Python environment, Espressif GCC/GDB toolchains, and the `esp` Rust toolchain as a
          # fixed-output derivation: shared via `nix build .#esp-idf --out-link $HOME/opt/esp-idf` rather than under target/.
          esp-idf = pkgs.stdenvNoCC.mkDerivation {
            name = "esp-idf-6.0.2";
            outputHashMode = "recursive";
            outputHashAlgo = "sha256";
            outputHash = "sha256-lyf0Coyf+U8TPiW5Tv9XY+GujEI/EwsiA1UxfZtGJG8=";
            nativeBuildInputs = [
              pkgs.bashInteractive
              pkgs.cmake
              pkgs.coreutils
              pkgs.curl
              pkgs.espup
              pkgs.findutils
              pkgs.git
              pkgs.gnused
              pkgs.ninja
              pkgs.python3
              pkgs.which
              pkgs.xz
            ];
            dontUnpack = true;
            buildPhase = ''
              runHook preBuild
              export HOME="$TMPDIR/home"
              export IDF_TOOLS_PATH="$out/espressif"
              export RUSTUP_HOME="$out/rustup"
              export CARGO_HOME="$TMPDIR/cargo"
              mkdir -p "$HOME" "$out" "$IDF_TOOLS_PATH" "$RUSTUP_HOME" "$CARGO_HOME"
              git clone --branch v6.0.2 --depth 1 --recursive --jobs 8 \
                https://github.com/espressif/esp-idf.git "$out/esp-idf"
              printf 'v6.0.2\n' > "$out/esp-idf/version.txt"
              find "$out/esp-idf" -name ".git" -exec rm -rf {} +
              "$out/esp-idf/install.sh" esp32,esp32s3,esp32c6
              espup install --targets esp32,esp32s3,esp32c6 --export-file "$out/export-esp.sh"
              cat > "$out/export-esp.sh" <<'EOF'
              export LIBCLANG_PATH="''${XIP_ESP_ROOT:-$HOME/opt/esp-idf}/rustup/toolchains/esp/xtensa-esp32-elf-clang/esp-20.1.1_20250829/esp-clang/lib"
              export PATH="''${XIP_ESP_ROOT:-$HOME/opt/esp-idf}/rustup/toolchains/esp/xtensa-esp-elf/esp-15.2.0_20250920/xtensa-esp-elf/bin:$PATH"
              EOF
              for venv in "$out"/espressif/python_env/idf*_env; do
                real_py="$(readlink -f "$venv/bin/python3")"
                ln -sfn "$real_py" "$venv/bin/python3"
                find "$venv" -name "__pycache__" -exec rm -rf {} +
                sed -i "1s|^#!$out/espressif/python_env/[^/]*/bin/python.*|#!/usr/bin/env python3|" "$venv"/bin/* 2>/dev/null || true
                sed -i "s|$out|\$HOME/opt/esp-idf|g" "$venv/pyvenv.cfg" "$venv"/bin/activate* 2>/dev/null || true
              done
              rm -rf "$out/espressif/dist"
              runHook postBuild
            '';
            dontInstall = true;
          };
          # The C and ESP tools: the host side of building firmware (cmake, ninja, python, cc, cbindgen, espup, espflash, mesh-api-gen,
          # ...). ESP-IDF and its cross toolchains (and the esp Rust toolchain) live in the `.#esp-idf` package (`$HOME/opt/esp-idf`).
          # The Rust/wasm tools are the separate `wasm-dev`.
          #   nix build .#esp-dev --out-link ${ESP_DEV:-$HOME/opt/esp-dev}      (scripts/esp-deps.sh does this; env.sh puts its bin/ on PATH)
          esp-dev = pkgs.symlinkJoin {
            name = "esp-xip-modules-esp-dev";
            paths = [
              self.packages.${system}.module-tools
            ] ++ pkgs.lib.optional (ssh-mesh.packages ? ${system}) ssh-mesh.packages.${system}.mesh-api-gen ++ [
              pkgs.bashInteractive
              pkgs.cmake
              pkgs.coreutils
              pkgs.rust-cbindgen
              pkgs.curl
              pkgs.findutils
              pkgs.gawk
              pkgs.git
              pkgs.gnugrep
              pkgs.gnumake
              pkgs.gnused
              pkgs.libusb1
              pkgs.ldproxy
              pkgs.espup
              pkgs.espflash
              pkgs.ninja
              pkgs.pkg-config
              pkgs.python3
              pkgs.stdenv.cc
              pkgs.unzip
              pkgs.which
              pkgs.xz
            ];
            meta.priority = 1;
          };
          # The service catalogs of this repository's components and modules (scripts/gen.sh), published as
          # <service>/<VERSION>/tools.json. Not checked in: the store build generates them (fixed VERSION for reproducibility)
          # and the dmesh-cli package installs them under its etc/schemas, the layout `mesh::catalog` searches.
          xip-schemas = pkgs.stdenvNoCC.mkDerivation {
            pname = "xip-schemas";
            version = "0.1.0";
            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter = path: type: !(builtins.elem (baseNameOf path) [ "target" ".git" ]);
            };
            nativeBuildInputs = [
              pkgs.python3
              pkgs.coreutils
              pkgs.gnugrep
            ] ++ pkgs.lib.optional (ssh-mesh.packages ? ${system}) ssh-mesh.packages.${system}.mesh-api-gen;
            XIP_RUNTIME_VERSION = "1";
            buildPhase = ''
              runHook preBuild
              scripts/gen.sh --schemas-only
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              mkdir -p $out/etc/schemas
              cp -r target/schemas/. $out/etc/schemas/
              chmod -R u+w $out/etc/schemas
              runHook postInstall
            '';
          };
          # dmesh-cli (crates/dmesh-cli, moved here from DMesh on 2026-10-20): talks to a device over UART/USB with the PPP framing,
          # UDP6 or NAN/NOW. Built from the crates/ workspace (one Cargo.lock); its DMesh libraries are the git dependencies pinned there.
          dmesh-cli = pkgs.rustPlatform.buildRustPackage {
            pname = "dmesh-cli";
            version = "0.1.0";
            src = pkgs.lib.cleanSourceWith {
              src = ./crates;
              # .cargo/config.toml redirects target-dir to this checkout's target/: not wanted in the store build
              filter = path: type: !(builtins.elem (builtins.baseNameOf (toString path)) [ "target" "result" ".cargo" ]);
            };
            cargoBuildFlags = [ "-p" "dmesh-cli" ];
            cargoLock = {
              lockFile = ./crates/Cargo.lock;
              allowBuiltinFetchGit = true;
            };
            nativeBuildInputs = [ pkgs.makeWrapper ];
            doCheck = false;
            # dmesh-server includes ../../lmesh/resources/tools.json, outside its own crate (a DMesh checkout has it; the per-crate vendor
            # directory does not): provide the same file at that relative place.
            preBuild = ''
              mkdir -p "$NIX_BUILD_TOP/cargo-vendor-dir/lmesh/resources"
              cp dmesh-cli/schemas/lmesh/tools.json "$NIX_BUILD_TOP/cargo-vendor-dir/lmesh/resources/tools.json"
            '';
            postInstall = ''
              mkdir -p "$out/etc"
              cp -r dmesh-cli/schemas "$out/etc/schemas"
              # the service catalogs of this repository's components and modules (scripts/gen.sh): xip-wifi, xip-sys, ...
              # as <service>/<VERSION>/tools.json, the layout `mesh::catalog` searches (the wrapper's MESH_SCHEMA_DIR root).
              cp -r ${self.packages.${system}.xip-schemas}/etc/schemas/. "$out/etc/schemas/"
              chmod -R u+w "$out/etc/schemas"
              wrapProgram "$out/bin/dmesh-cli" --set-default MESH_SCHEMA_DIR "$out/etc/schemas"
            '';
            meta.mainProgram = "dmesh-cli";
          };
          # The DMesh tools: dmesh-cli, DMesh's other host tools (lmesh, mesh-tun, dmeshtui) and ssh-mesh's commands
          # (the `mesh` client in particular). Kept out of `deps` because they build the DMesh and ssh-mesh workspaces.
          # `scripts/esp-deps.sh dmesh` = `nix build .#dmesh --out-link ${ESP_DEV:-$HOME/opt/esp-dev}-dmesh`; env.sh puts its bin/ on PATH.
          dmesh = pkgs.symlinkJoin {
            name = "xip-dmesh";
            paths = [ self.packages.${system}.dmesh-cli dmesh.packages.${system}.dmesh ssh-mesh.packages.${system}.ssh-mesh ];
          };
          # Tools to compile the modules (modules/*) to WebAssembly and nothing else: Rust (nightly, as the modules use
          # allocator_api and RUSTC_BOOTSTRAP) with the wasm32 targets and rust-src (rust-lld is part of it), plus binaryen
          # (wasm-opt), wabt, wasm-tools and wasmtime to optimize, inspect and run the result (and gcc for the C of a host build).
          # Separate from `esp-dev`: that one is ESP-IDF and C.
          # modules/mod_wasm3/WASM.md:  nix build .#wasm-dev --out-link ${WASM_DEV:-$HOME/opt/wasm-dev}
          wasm-dev =
            let
              rpkgs = import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
              rust = rpkgs.rust-bin.nightly.latest.minimal.override {
                extensions = [ "rust-src" "clippy" ];
                targets = [ "x86_64-unknown-linux-musl" "wasm32-unknown-unknown" "wasm32v1-none" "wasm32-wasip1" ];
              };
            in
            rpkgs.symlinkJoin {
              name = "xip-wasm-dev";
              paths = [ rust rpkgs.binaryen rpkgs.wabt rpkgs.wasm-tools rpkgs.wasmtime rpkgs.gcc ];
            };
          # the old name of esp-dev
          deps = self.packages.${system}.esp-dev;
          default = self.packages.${system}.esp-dev;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            packages = [ self.packages.${system}.esp-dev ];
            shellHook = ''
              [ -f ./env.sh ] && . ./env.sh
            '';
          };
        }
      );
    };
}
