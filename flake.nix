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
          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          gitRev =
            if self ? shortRev then
              self.shortRev
            else if self ? dirtyShortRev then
              self.dirtyShortRev
            else
              "";
          herdr-tui = pkgs.rustPlatform.buildRustPackage {
            pname = cargoToml.package.name;
            version = cargoToml.package.version;
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
            };
            env = lib.optionalAttrs (gitRev != "") {
              HCT_GIT_SHA = gitRev;
            };
            meta = with lib; {
              inherit (cargoToml.package) description;
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
