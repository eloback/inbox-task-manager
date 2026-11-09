{
  description = "Inbox-style task manager on top of Herdr named sessions";

  # Herdr is packaged in nixos-unstable.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # itm runs on Linux (NixOS included); DESIGN.md section 4.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.inbox-task-manager;
        inbox-task-manager = pkgs.callPackage ./nix/package.nix { };
      });

      overlays.default = final: _prev: {
        inbox-task-manager = final.callPackage ./nix/package.nix { };
      };

      homeModules.default = import ./nix/home-module.nix self;

      checks = forAllSystems (
        pkgs:
        let
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in
        {
          # Building runs the test suite (checkPhase: cargo test).
          inherit package;
          # The rest of the quality bar (DESIGN.md section 6): clippy with
          # warnings denied, on the same sources and crates.
          clippy = package.overrideAttrs (old: {
            pname = "${old.pname}-clippy";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
            buildPhase = "cargo clippy --all-targets --offline -- -D warnings";
            doCheck = false;
            installPhase = "touch $out";
            postInstall = "";
          });
          fmt = pkgs.runCommand "inbox-task-manager-fmt" { nativeBuildInputs = [ pkgs.rustfmt ]; } ''
            cd ${package.src}
            rustfmt --check --edition 2024 src/main.rs tests/switching.rs
            touch $out
          '';
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          # The Rust toolchain, and what the end-to-end scripts drive
          # (scripts/smoke.sh, measure.sh: tmux, socat, python3).
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.git
            pkgs.tmux
            pkgs.socat
            pkgs.python3
          ]
          ++ pkgs.lib.optional (pkgs.lib.meta.availableOn pkgs.stdenv.hostPlatform pkgs.herdr) pkgs.herdr;
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
