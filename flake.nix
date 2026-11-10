{
  description = "herdr-tui (hct) — Multi-tab TUI client for herdr-core (hc)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          herdr-tui = pkgs.rustPlatform.buildRustPackage {
            pname = "herdr-tui";
            version = "0.1.0";
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
            };
            meta = with lib; {
              description = "Lightweight multi-tab TUI client for herdr-core (hc) with bidirectional Space/Tab sync";
              license = licenses.asl20;
              mainProgram = "hct";
            };
          };
        in
        {
          inherit herdr-tui;
          default = herdr-tui;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/hct";
        };
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            name = "herdr-tui-dev";
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer
            ];
          };
        }
      );
    };
}
