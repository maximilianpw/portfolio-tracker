{
  description = "portfolio-tracker - read-only Trade Republic tracker";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        inherit (pkgs) lib;

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        commonArgs = {
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              (craneLib.fileset.commonCargoSources ./.)
              ./migrations
              ./.sqlx
            ];
          };
          strictDeps = true;
          SQLX_OFFLINE = "true";
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        portfolio-tracker = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            doCheck = false;
            meta.mainProgram = "portfolio-tracker";
          }
        );
      in
      {
        packages.default = portfolio-tracker;

        checks = {
          inherit portfolio-tracker;
          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );
          test = craneLib.cargoTest (commonArgs // { inherit cargoArtifacts; });
          fmt = craneLib.cargoFmt { inherit (commonArgs) src; };
        };

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};
          packages = with pkgs; [
            pytr
            sqlx-cli
            sqlite
            jq
          ];

          # sqlx's query macros check SQL against this database; `cargo sqlx prepare` refreshes .sqlx/ from it.
          shellHook = ''
            export DATABASE_URL="sqlite://$PWD/target/sqlx-dev.sqlite"
          '';
        };
      }
    )
    // {
      nixosModules.default = import ./nix/module.nix self;
    };
}
