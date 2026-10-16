{
  description = "esp-xip-modules host build dependencies";

  # The nixpkgs pin in flake.lock is kept equal to esp-wifi-loader's (same host tools for both; `nix flake lock
  # --override-input` or copying its flake.lock realigns them). Nothing here depends on DMesh.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
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
              mkdir -p $out/share/esp-xip-modules/{scripts,modules,crates} $out/bin
              cp scripts/build-mod.sh $out/share/esp-xip-modules/scripts/
              cp -r tools include abi $out/share/esp-xip-modules/
              cp -r modules/common $out/share/esp-xip-modules/modules/
              cp -r crates/xip-abi $out/share/esp-xip-modules/crates/
              mkdir -p $out/share/esp-xip-modules/main
              cp main/main.c $out/share/esp-xip-modules/main/
              mkdir -p $out/share/esp-xip-modules/crates/xip-runtime-esp/src
              cp crates/xip-runtime-esp/src/loader.rs $out/share/esp-xip-modules/crates/xip-runtime-esp/src/
              makeWrapper $out/share/esp-xip-modules/scripts/build-mod.sh $out/bin/build-mod.sh \
                --set XIP_TOOL_ROOT $out/share/esp-xip-modules \
                --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.python3 pkgs.bash pkgs.coreutils ]}
            '';
          };
          # Host tools only. ESP-IDF and its cross toolchains (and the esp Rust toolchain) are fetched by
          # scripts/esp-deps.sh into target/.
          deps = pkgs.symlinkJoin {
            name = "esp-xip-modules-deps";
            paths = [
              self.packages.${system}.module-tools
              pkgs.bashInteractive
              pkgs.cmake
              pkgs.coreutils
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
              pkgs.ninja
              pkgs.openssl # signing, tests
              pkgs.pkg-config
              pkgs.python3
              pkgs.stdenv.cc
              pkgs.unzip
              pkgs.which
              pkgs.xz
            ];
            meta.priority = 1;
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
