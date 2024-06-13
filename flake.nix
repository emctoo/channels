{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };

    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs = inputs@{ self, fenix, crane, flake-parts, advisory-db, ... }:
    flake-parts.lib.mkFlake { inherit self inputs; } ({ withSystem, ... }: {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      perSystem = { lib, config, self', inputs', pkgs, system, ... }:
        let
          rustToolchain = fenix.packages.${system}.stable.withComponents [
            "rustc"
            "cargo"
            "rustfmt"
            "clippy"
            "rust-src"
          ];

          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          commonArgs = {
            src = craneLib.cleanCargoSource ./.;
            pname = "websocket-channel";
            version = "v0.1.0";
            nativeBuildInputs = with pkgs; [ pkg-config openssl clang mold python3];
            buildInputs = [ ] ++ lib.optionals pkgs.stdenv.isDarwin [
              pkgs.libiconv
            ];

            RUSTFLAGS="-C linker=clang -C link-arg=-fuse-ld=${pkgs.mold}/bin/mold";
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        in
        {
          devShells.default = pkgs.mkShell {
            inputsFrom = builtins.attrValues self.checks;
            buildInputs = [ rustToolchain pkgs.pkg-config pkgs.openssl pkgs.cargo-watch ];
          };

          packages =
            {
              default =  craneLib.buildPackage (commonArgs // {  # TODO  this does not build for now
                pname = "channel";
                cargoExtraFlags = "";
                meta.mainProgram = "channel";
                inherit cargoArtifacts;
              });

              # docs = pkgs.callPackage ./docs {};
            };

          checks =
            {
              fmt = craneLib.cargoFmt (commonArgs);
              audit = craneLib.cargoAudit (commonArgs // { inherit advisory-db; });
              rustdoc = craneLib.cargoDoc (commonArgs // { inherit cargoArtifacts; });

              clippy-check = craneLib.cargoClippy (commonArgs // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-features -- --deny warnings";
              });

              test-check = craneLib.cargoNextest (commonArgs // {
                inherit cargoArtifacts;
                partitions = 1;
                partitionType = "count";
              });
            }
            # build packages as part of the checks
            // (lib.mapAttrs' (key: value: lib.nameValuePair (key + "-package") value) self'.packages);

          formatter = pkgs.nixpkgs-fmt;
        };
    });

  nixConfig = {
    extra-trusted-substituters = [ "https://nix-community.cachix.org" ];
    extra-trusted-public-keys = [ "nix-community.cachix.org-1:mB9FSh9qf2dCimDSUo8Zy7bkq5CX+/rkCWyvRCYg3Fs=" ];
  };
}
