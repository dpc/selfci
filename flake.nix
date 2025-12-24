{
  description = "SelfCI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
    flakebox.url = "github:rustshop/flakebox?rev=9a22c690bc3c15291c3c70f662c855b5bdaffc0e";

    bundlers = {
      url = "github:NixOS/bundlers";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      flakebox,
      bundlers,
    }:
    {
      bundlers = bundlers.bundlers;
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        projectName = "selfci";

        flakeboxLib = flakebox.lib.mkLib pkgs {
          config = {
            github.ci.buildOutputs = [ ".#ci.${projectName}" ];
            just.importPaths = [ "justfile.selfci.just" ];
            toolchain.channel = "latest";
            rust.rustfmt.enable = false;
            linker.wild.enable = true;
          };
        };

        toolchainArgs = {
          extraRustFlags = "-Z threads=0";
        };

        stdToolchains = (flakeboxLib.mkStdToolchains (toolchainArgs // { }));

        toolchainAll = (
          flakeboxLib.mkFenixToolchain (
            toolchainArgs
            // {
              targets = pkgs.lib.getAttrs [ "default" ] (flakeboxLib.mkStdTargets { });
            }
          )
        );

        buildPaths = [
          "Cargo.toml"
          "Cargo.lock"
          "src"
          "share"
          ".*\.rs"
        ];

        buildSrc = flakeboxLib.filterSubPaths {
          root = builtins.path {
            name = projectName;
            path = ./.;
          };
          paths = buildPaths;
        };

        multiBuild =
          (flakeboxLib.craneMultiBuild {
            toolchains = stdToolchains;
          })
            (
              craneLib':
              let
                craneLib = (
                  craneLib'.overrideArgs {
                    pname = projectName;
                    src = buildSrc;
                    nativeBuildInputs = [ ];
                  }
                );
              in
              rec {
                workspaceDeps = craneLib.buildWorkspaceDepsOnly { };

                workspace = craneLib.buildWorkspace {
                  cargoArtifacts = workspaceDeps;
                };

                selfci = craneLib.buildPackage {
                  cargoArtifacts = workspace;
                  meta.mainProgram = "selfci";
                };

                tests = craneLib.cargoNextest {
                  cargoArtifacts = workspace;
                };

                clippy = craneLib.cargoClippy {
                  cargoArtifacts = workspace;
                };
              }
            );
        selfci = multiBuild.selfci;
      in
      {
        packages = {
          inherit selfci;
          default = selfci;
        };

        legacyPackages = multiBuild;

        devShells = flakeboxLib.mkShells {
          toolchain = toolchainAll;
          packages = [ ];
        };
      }
    );
}
