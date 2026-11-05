{
  description = "Inbox-style task manager on top of Herdr named sessions";

  # Herdr is packaged in nixos-unstable.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # nixpkgs 26.11 dropped x86_64-darwin; the overlay and the home-manager
      # module still build itm there from your own nixpkgs.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
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

      checks = forAllSystems (pkgs: {
        # Building runs the test suite (installCheckPhase).
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            (pkgs.python3.withPackages (ps: [ ps.textual ]))
            pkgs.git
          ]
          ++ pkgs.lib.optional (pkgs.lib.meta.availableOn pkgs.stdenv.hostPlatform pkgs.herdr) pkgs.herdr;
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
