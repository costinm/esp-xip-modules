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
  };

  outputs =
    { self, nixpkgs, ssh-mesh, dmesh }:
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
          # Host tools only. ESP-IDF and its cross toolchains (and the esp Rust toolchain) are fetched by
          # scripts/esp-deps.sh into target/.
          deps = pkgs.symlinkJoin {
            name = "esp-xip-modules-deps";
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
              # the service catalogs of this repository's components and modules (scripts/gen.sh): xip-wifi, xip-power, xip-con, ...
              cp -r ${./api/generated/schemas}/. "$out/etc/schemas/"
              chmod -R u+w "$out/etc/schemas"
              wrapProgram "$out/bin/dmesh-cli" --set-default MESH_SCHEMA_DIR "$out/etc/schemas"
            '';
            meta.mainProgram = "dmesh-cli";
          };
          # The DMesh tools: dmesh-cli, DMesh's other host tools (lmesh, mesh-tun, dmeshtui) and ssh-mesh's commands
          # (the `mesh` client in particular). Kept out of `deps` because they build the DMesh and ssh-mesh workspaces.
          # `scripts/esp-deps.sh dmesh` = `nix build --out-link target/nix/dmesh`; env.sh puts its bin/ on PATH.
          dmesh = pkgs.symlinkJoin {
            name = "xip-dmesh";
            paths = [ self.packages.${system}.dmesh-cli dmesh.packages.${system}.dmesh ssh-mesh.packages.${system}.ssh-mesh ];
          };
          default = self.packages.${system}.deps;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            packages = [ self.packages.${system}.deps ];
            shellHook = ''
              [ -f ./env.sh ] && . ./env.sh
            '';
          };
        }
      );
    };
}
