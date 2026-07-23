{
  description = "SelfCI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    nixpkgs-unstable.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    flakebox.url = "github:rustshop/flakebox?rev=62af969ab344229d2a0d585a482293b3f186b221";

    bundlers = {
      url = "github:NixOS/bundlers";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-unstable,
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
        unstablePkgs = nixpkgs-unstable.legacyPackages.${system};
        projectName = "selfci";

        flakeboxLib = flakebox.lib.mkLib pkgs {
          config = {
            github.ci.buildOutputs = [
              ".#ci.${projectName}"
              ".#ci.tests"
            ];
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
          "tests"
          ".*\.rs"
          "build.rs"
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
                  doInstallCargoArtifacts = false;
                  nativeBuildInputs = with pkgs; [
                    git
                    unstablePkgs.jujutsu
                  ];
                  env = {
                    NEXTEST_SHOW_PROGRESS = "none";
                  };
                };

                clippy = craneLib.cargoClippy {
                  # must be deps, otherwise it will not rebuild
                  # anything and thus not detect anything
                  cargoArtifacts = workspaceDeps;
                  doInstallCargoArtifacts = false;
                  cargoClippyExtraArgs = "--all-targets -- -D warnings";
                };
              }
            );
        selfci = multiBuild.selfci;
        mq = pkgs.writeShellScriptBin "mq" ''
          exec ${selfci}/bin/selfci mq add --wait "$@"
        '';
      in
      {
        packages = {
          inherit selfci mq;
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
